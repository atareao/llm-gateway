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
use serde_json::{Value, json};
use tracing::Instrument;

use crate::config::{Backend, GatewayConfig, Route};
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

        Ok(Self {
            config: Arc::new(config),
            client,
            metrics,
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
    format!("{}#{}", backend.base_url, backend.model)
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

    let span = tracing::info_span!(
        "request",
        request_id = %request_id,
        route = %route.name,
        streaming
    );
    let result = route_with_failover(&state, route_idx, route, &body, streaming)
        .instrument(span)
        .await;

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
/// En modo `streaming`, al primer backend OK se devuelve su stream sin
/// consumir; a partir de ahí ya no hay failover.
async fn route_with_failover(
    state: &AppState,
    route_idx: usize,
    route: &Route,
    body: &Value,
    streaming: bool,
) -> Result<Response, AppError> {
    let started = Instant::now();
    let mut attempts = 0usize;
    let mut considered = 0usize;
    // Se decide una sola vez por petición y solo aplica en no-stream.
    let requires_tool_call = !streaming && request_requires_tool_call(body);
    let mut no_tool_call_seen = false;

    for (backend_idx, backend) in route.backend.iter().enumerate() {
        considered += 1;
        let attempt_started = Instant::now();

        let Some(api_key) = resolve_api_key(backend) else {
            state.metrics.record_attempt(
                route_idx,
                backend_idx,
                AttemptResult::MissingApiKey,
                attempt_started.elapsed(),
            );
            tracing::warn!(
                backend = %backend_id(backend),
                result = AttemptResult::MissingApiKey.label(),
                latency_ms = attempt_started.elapsed().as_millis() as u64,
                "backend sin api_key; probando el siguiente"
            );
            continue;
        };
        attempts += 1;

        let payload = body_for_backend(body, backend);
        match send(
            &state.client,
            backend,
            payload,
            streaming,
            api_key.as_deref(),
        )
        .await
        {
            Ok(response) if is_retryable(response.status()) => {
                state.metrics.record_attempt(
                    route_idx,
                    backend_idx,
                    AttemptResult::RetryableStatus,
                    attempt_started.elapsed(),
                );
                tracing::warn!(
                    backend = %backend_id(backend),
                    status = %response.status(),
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
                        backend = %backend_id(backend),
                        result = AttemptResult::Success.label(),
                        latency_ms = attempt_started.elapsed().as_millis() as u64,
                        "intento completado"
                    );
                    if attempts > 1 {
                        state.metrics.record_failover(route_idx);
                    }
                    tracing::info!(
                        outcome = Outcome::Success.label(),
                        attempts,
                        latency_ms = started.elapsed().as_millis() as u64,
                        "petición completada"
                    );
                    return Ok(stream_response(response, backend));
                }

                match full_response(response, backend).await {
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
                                backend = %backend_id(backend),
                                result = AttemptResult::NoToolCall.label(),
                                latency_ms = attempt_started.elapsed().as_millis() as u64,
                                "respuesta sin tool_calls cuando la petición los exige; probando el siguiente"
                            );
                            no_tool_call_seen = true;
                            continue;
                        }
                        state.metrics.record_attempt(
                            route_idx,
                            backend_idx,
                            AttemptResult::Success,
                            attempt_started.elapsed(),
                        );
                        tracing::debug!(
                            backend = %backend_id(backend),
                            result = AttemptResult::Success.label(),
                            latency_ms = attempt_started.elapsed().as_millis() as u64,
                            "intento completado"
                        );
                        if attempts > 1 {
                            state.metrics.record_failover(route_idx);
                        }
                        tracing::info!(
                            outcome = Outcome::Success.label(),
                            attempts,
                            latency_ms = started.elapsed().as_millis() as u64,
                            "petición completada"
                        );
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
                            backend = %backend_id(backend),
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
                    backend = %backend_id(backend),
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
        outcome = Outcome::AllFailed.label(),
        attempts,
        considered,
        latency_ms = started.elapsed().as_millis() as u64,
        "petición fallida"
    );

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
    match backend.api_key_env.as_deref() {
        None => Some(None),
        Some(var) => match std::env::var(var) {
            Ok(value) if !value.is_empty() => Some(Some(value)),
            _ => {
                tracing::warn!(
                    backend = %backend_id(backend),
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

/// Envía la petición al backend y devuelve la respuesta con las cabeceras ya
/// disponibles, **sin** consumir el cuerpo (permite decidir el failover antes
/// del primer byte). Si `api_key` es `Some`, añade `Authorization: Bearer`.
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
    backend: &Backend,
    payload: Value,
    streaming: bool,
    api_key: Option<&str>,
) -> Result<reqwest::Response, SendError> {
    let mut request = client.post(chat_url(&backend.base_url)).json(&payload);
    if let Some(key) = api_key {
        request = request.bearer_auth(key);
    }

    if streaming {
        let budget = backend
            .timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_FIRST_BYTE_TIMEOUT);
        match tokio::time::timeout(budget, request.send()).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(err)) => Err(SendError::Transport(err)),
            Err(_elapsed) => Err(SendError::FirstByteTimeout(budget)),
        }
    } else {
        let budget = backend
            .timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_REQUEST_TIMEOUT);
        request = request.timeout(budget);
        request.send().await.map_err(SendError::Transport)
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
    backend: &Backend,
) -> Result<(Response, Bytes), AppError> {
    let status = response.status();
    let content_type = response.headers().get(header::CONTENT_TYPE).cloned();
    let body = response.bytes().await.map_err(AppError::Upstream)?;

    let response = assemble(
        status,
        content_type,
        Body::from(body.clone()),
        DEFAULT_CONTENT_TYPE,
        backend,
    );

    Ok((response, body))
}

/// Reenvía el cuerpo del upstream como stream SSE, **sin bufferizar**, usando
/// el stream de `reqwest` directamente.
///
/// Si el upstream se corta a mitad, el error del stream propaga al cliente y
/// la conexión se cierra con error; no se reintenta (limitación documentada).
fn stream_response(response: reqwest::Response, backend: &Backend) -> Response {
    let status = response.status();
    let content_type = response.headers().get(header::CONTENT_TYPE).cloned();
    let body = Body::from_stream(response.bytes_stream());

    assemble(
        status,
        content_type,
        body,
        DEFAULT_STREAM_CONTENT_TYPE,
        backend,
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
    backend: &Backend,
) -> Response {
    let content_type =
        content_type.unwrap_or_else(|| HeaderValue::from_static(default_content_type));

    let mut response = (status, [(header::CONTENT_TYPE, content_type)], body).into_response();

    if status.is_success()
        && let Ok(value) = HeaderValue::from_str(&backend_id(backend))
    {
        response
            .headers_mut()
            .insert(HeaderName::from_static(BACKEND_HEADER), value);
    }

    response
}

/// Clona `body` y sustituye su `model` por el del backend. El resto de campos
/// (incluidos los desconocidos) se conservan intactos.
pub(crate) fn body_for_backend(body: &Value, backend: &Backend) -> Value {
    let mut value = body.clone();
    if let Some(object) = value.as_object_mut() {
        object.insert("model".to_owned(), Value::String(backend.model.clone()));
    }
    value
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

/// `GET /v1/models`: lista los nombres lógicos de ruta como modelos.
async fn list_models(State(state): State<AppState>) -> Json<Value> {
    let data: Vec<Value> = state
        .config
        .route
        .iter()
        .map(|route| json!({"id": route.name, "object": "model"}))
        .collect();

    Json(json!({"object": "list", "data": data}))
}

/// `GET /healthz`: sondeo de salud.
async fn healthz() -> &'static str {
    "ok"
}

#[cfg(test)]
mod tests {
    use super::{chat_url, request_requires_tool_call, response_has_tool_call};
    use serde_json::json;

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
}
