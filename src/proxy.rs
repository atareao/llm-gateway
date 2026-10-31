//! Router y handler del endpoint compatible con OpenAI.
//!
//! Esta capa no interpreta el cuerpo: lo reenvía tal cual **salvo `model`**,
//! que se sustituye por el del backend al que se enruta la petición.

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::Value;

use crate::config::{Backend, GatewayConfig};
use crate::error::AppError;

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
    /// Se crea un [`reqwest::Client`] nuevo con la configuración por defecto.
    pub fn new(config: GatewayConfig) -> Self {
        Self {
            config: Arc::new(config),
            client: reqwest::Client::new(),
        }
    }
}

/// Construye el router de la aplicación con el endpoint de chat.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .with_state(state)
}

/// `POST /v1/chat/completions` (no-streaming).
///
/// Resuelve la ruta lógica a partir del campo `model`, reescribe `model` con el
/// del backend primario y reenvía el resto del cuerpo verbatim, devolviendo la
/// respuesta del upstream con su mismo status.
async fn chat_completions(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Result<Response, AppError> {
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::ModelNotFound("<missing>".to_owned()))?;

    let route = state
        .config
        .route
        .iter()
        .find(|route| route.name == model)
        .ok_or_else(|| AppError::ModelNotFound(model.to_owned()))?;

    // El bucle de failover (lote C) seleccionará aquí el primer backend
    // disponible; de momento basta con el primario.
    let backend = route.backend.first().ok_or_else(|| {
        AppError::Upstream(format!("la ruta '{model}' no tiene backends configurados"))
    })?;

    forward(&state.client, backend, body).await
}

/// Reenvía `body` al backend indicado, con `model` reescrito, y devuelve su
/// respuesta (status + JSON) tal cual.
async fn forward(
    client: &reqwest::Client,
    backend: &Backend,
    mut body: Value,
) -> Result<Response, AppError> {
    if let Some(object) = body.as_object_mut() {
        object.insert("model".to_owned(), Value::String(backend.model.clone()));
    }

    let mut request = client.post(chat_url(&backend.base_url)).json(&body);
    if let Some(ms) = backend.timeout_ms {
        request = request.timeout(Duration::from_millis(ms));
    }

    let upstream = request
        .send()
        .await
        .map_err(|err| AppError::Upstream(err.to_string()))?;

    let status = upstream.status();
    let body = upstream
        .bytes()
        .await
        .map_err(|err| AppError::Upstream(err.to_string()))?;

    Ok(json_response(status, body))
}

/// Compone la URL del endpoint de chat a partir de la `base_url` del backend
/// (que ya incluye `/v1`), uniendo bien las barras.
fn chat_url(base_url: &str) -> String {
    format!("{}/chat/completions", base_url.trim_end_matches('/'))
}

/// Construye una respuesta JSON con el status indicado.
fn json_response(status: StatusCode, body: Bytes) -> Response {
    (status, [(header::CONTENT_TYPE, "application/json")], body).into_response()
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
