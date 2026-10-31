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
//! ### Limitación de streaming
//! En modo `stream: true`, una vez empezada la emisión **no hay failover**: si
//! el upstream se corta a mitad, el stream se cierra con error (no se
//! reintenta, porque ya se habrían enviado bytes al cliente).

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::{Value, json};

use crate::config::{Backend, GatewayConfig};
use crate::error::AppError;

/// Timeout de establecimiento de conexión hacia un backend, para que un
/// backend caído no deje la petición colgada antes de decidir el failover.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(4);

/// Nombre de la cabecera que identifica el backend que atendió la petición.
pub const BACKEND_HEADER: &str = "x-gateway-backend";

/// `content-type` por defecto para respuestas no-stream.
const DEFAULT_CONTENT_TYPE: &str = "application/json";

/// `content-type` por defecto para respuestas en streaming.
const DEFAULT_STREAM_CONTENT_TYPE: &str = "text/event-stream";

/// Estado compartido por los handlers: configuración de rutas y cliente HTTP
/// reutilizable (con pool de conexiones) hacia los backends.
#[derive(Clone)]
pub struct AppState {
    /// Configuración de rutas cargada al arrancar.
    pub config: Arc<GatewayConfig>,
    /// Cliente HTTP compartido hacia los backends.
    pub client: reqwest::Client,
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

        Ok(Self {
            config: Arc::new(config),
            client,
        })
    }
}

/// Construye el router de la aplicación.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/models", get(list_models))
        .route("/healthz", get(healthz))
        .with_state(state)
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
/// Resuelve la ruta lógica a partir del campo `model` y recorre sus backends en
/// orden hasta obtener una respuesta válida, reescribiendo `model` con el del
/// backend elegido y reenviando el resto del cuerpo verbatim. Si el cuerpo trae
/// `stream: true`, la respuesta del upstream se reenvía sin bufferizar.
async fn chat_completions(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Result<Response, AppError> {
    let model = body.get("model").and_then(Value::as_str).ok_or_else(|| {
        AppError::InvalidRequest("el campo 'model' es obligatorio y debe ser una cadena".to_owned())
    })?;

    let route = state
        .config
        .route
        .iter()
        .find(|route| route.name == model)
        .ok_or_else(|| AppError::ModelNotFound(model.to_owned()))?;

    let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);

    let mut attempts = 0usize;
    for backend in &route.backend {
        attempts += 1;

        let payload = body_for_backend(&body, backend);
        match send(&state.client, backend, payload, streaming).await {
            Ok(response) if is_retryable(response.status()) => {
                tracing::warn!(
                    backend = %backend_id(backend),
                    status = %response.status(),
                    "backend retornó un status reintentable; probando el siguiente"
                );
            }
            Ok(response) => {
                if streaming {
                    // A partir de aquí ya se emiten bytes: sin failover.
                    return Ok(stream_response(response, backend));
                }

                match full_response(response, backend).await {
                    Ok(response) => return Ok(response),
                    Err(err) => tracing::warn!(
                        backend = %backend_id(backend),
                        error = %err,
                        "error leyendo la respuesta del backend; probando el siguiente"
                    ),
                }
            }
            Err(err) => {
                tracing::warn!(
                    backend = %backend_id(backend),
                    error = %err,
                    "backend inalcanzable; probando el siguiente"
                );
            }
        }
    }

    Err(AppError::AllBackendsFailed {
        model: model.to_owned(),
        attempts,
    })
}

/// Indica si un status del upstream permite reintentar con otro backend
/// (`429` o `5xx`); un `4xx` distinto de `429` no se reintenta.
fn is_retryable(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

/// Envía la petición al backend y devuelve la respuesta con las cabeceras ya
/// disponibles, **sin** consumir el cuerpo (permite decidir el failover antes
/// del primer byte).
///
/// El `timeout_ms` por backend se aplica solo a peticiones no-stream: en
/// streaming, un timeout global abortaría respuestas largas legítimas.
async fn send(
    client: &reqwest::Client,
    backend: &Backend,
    payload: Value,
    streaming: bool,
) -> Result<reqwest::Response, reqwest::Error> {
    let mut request = client.post(chat_url(&backend.base_url)).json(&payload);
    if !streaming && let Some(ms) = backend.timeout_ms {
        request = request.timeout(Duration::from_millis(ms));
    }
    request.send().await
}

/// Convierte la respuesta del upstream en una respuesta no-stream (buffered).
async fn full_response(
    response: reqwest::Response,
    backend: &Backend,
) -> Result<Response, AppError> {
    let status = response.status();
    let content_type = response.headers().get(header::CONTENT_TYPE).cloned();
    let body = response.bytes().await.map_err(AppError::Upstream)?;

    Ok(assemble(
        status,
        content_type,
        Body::from(body),
        DEFAULT_CONTENT_TYPE,
        backend,
    ))
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
fn body_for_backend(body: &Value, backend: &Backend) -> Value {
    let mut value = body.clone();
    if let Some(object) = value.as_object_mut() {
        object.insert("model".to_owned(), Value::String(backend.model.clone()));
    }
    value
}

/// Compone la URL del endpoint de chat a partir de la `base_url` del backend
/// (que ya incluye `/v1`), uniendo bien las barras.
fn chat_url(base_url: &str) -> String {
    format!("{}/chat/completions", base_url.trim_end_matches('/'))
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
    use super::chat_url;

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
}
