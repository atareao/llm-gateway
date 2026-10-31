//! Router y handler del endpoint compatible con OpenAI.
//!
//! Esta capa no interpreta el cuerpo: lo reenvía tal cual **salvo `model`**,
//! que se sustituye por el del backend al que se enruta la petición.
//!
//! El enrutado recorre la cadena ordenada de backends de la ruta y hace
//! **failover antes del primer byte**: si un intento falla por conexión,
//! timeout, `429` o `5xx`, se prueba el siguiente; un `4xx` distinto de `429`
//! se devuelve al cliente sin reintentar.

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::Value;

use crate::config::{Backend, GatewayConfig};
use crate::error::AppError;

/// Timeout de establecimiento de conexión hacia un backend, para que un
/// backend caído no deje la petición colgada antes de decidir el failover.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(4);

/// Nombre de la cabecera que identifica el backend que atendió la petición.
pub const BACKEND_HEADER: &str = "x-gateway-backend";

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

/// Construye el router de la aplicación con el endpoint de chat.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/chat/completions", post(chat_completions))
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

/// `POST /v1/chat/completions` (no-streaming).
///
/// Resuelve la ruta lógica a partir del campo `model` y recorre sus backends en
/// orden hasta obtener una respuesta válida, reescribiendo `model` con el del
/// backend elegido y reenviando el resto del cuerpo verbatim.
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

    let mut attempts = 0usize;
    for backend in &route.backend {
        attempts += 1;

        match forward(&state.client, backend, &body).await {
            Ok(upstream) if is_retryable(upstream.status) => {
                tracing::warn!(
                    backend = %backend_id(backend),
                    status = %upstream.status,
                    "backend retornó un status reintentable; probando el siguiente"
                );
            }
            Ok(upstream) => {
                // Éxito (2xx) o error no reintentable (4xx): se devuelve tal cual.
                return Ok(response_from(upstream, backend));
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

/// Resultado de un intento contra un backend, listo para reenviar al cliente.
struct Upstream {
    status: StatusCode,
    content_type: Option<HeaderValue>,
    body: Bytes,
}

/// Reenvía `body` al backend indicado, con `model` reescrito, y devuelve su
/// respuesta (status, `content-type` y cuerpo) sin interpretarla.
async fn forward(
    client: &reqwest::Client,
    backend: &Backend,
    body: &Value,
) -> Result<Upstream, AppError> {
    let payload = body_for_backend(body, backend);

    let mut request = client.post(chat_url(&backend.base_url)).json(&payload);
    if let Some(ms) = backend.timeout_ms {
        request = request.timeout(Duration::from_millis(ms));
    }

    let response = request.send().await.map_err(AppError::Upstream)?;
    let status = response.status();
    let content_type = response.headers().get(header::CONTENT_TYPE).cloned();
    let body = response.bytes().await.map_err(AppError::Upstream)?;

    Ok(Upstream {
        status,
        content_type,
        body,
    })
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

/// Construye la respuesta para el cliente preservando el `content-type` del
/// upstream (o `application/json` si no lo envía). En caso de éxito añade la
/// cabecera `x-gateway-backend` con el identificador del backend usado.
fn response_from(upstream: Upstream, backend: &Backend) -> Response {
    let content_type = upstream
        .content_type
        .unwrap_or_else(|| HeaderValue::from_static("application/json"));

    let mut response = (
        upstream.status,
        [(header::CONTENT_TYPE, content_type)],
        upstream.body,
    )
        .into_response();

    if upstream.status.is_success()
        && let Ok(value) = HeaderValue::from_str(&backend_id(backend))
    {
        response
            .headers_mut()
            .insert(HeaderName::from_static(BACKEND_HEADER), value);
    }

    response
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
