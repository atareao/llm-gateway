//! Error tipado del gateway, con respuesta JSON en el formato de OpenAI.
//!
//! El cuerpo de error sigue la forma
//! `{"error": {"message": ..., "type": ..., "code": ...}}`, idéntica a la de
//! la API de OpenAI para que los clientes existentes la interpreten sin cambios.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

/// Error de la capa HTTP del gateway.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// La petición no trae un `model` utilizable (ausente o no-string).
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    /// El `model` pedido por el cliente no corresponde a ninguna ruta lógica.
    #[error("the model '{0}' does not exist")]
    ModelNotFound(String),

    /// Fallo de transporte hacia un backend (conexión, timeout, lectura).
    ///
    /// Se conserva el `reqwest::Error` como `source` para no perder la cadena
    /// de causas; el failover decide si puede reintentar con otro backend.
    #[error("upstream backend error")]
    Upstream(#[source] reqwest::Error),

    /// Todos los backends de la cadena fallaron antes del primer byte.
    #[error("all {attempts} backend(s) failed for model '{model}'")]
    AllBackendsFailed { model: String, attempts: usize },
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, error_type, code) = match &self {
            AppError::InvalidRequest(_) => (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "invalid_request",
            ),
            AppError::ModelNotFound(_) => (
                StatusCode::NOT_FOUND,
                "invalid_request_error",
                "model_not_found",
            ),
            AppError::Upstream(_) | AppError::AllBackendsFailed { .. } => {
                (StatusCode::BAD_GATEWAY, "api_error", "upstream_error")
            }
        };

        let body = Json(json!({
            "error": {
                "message": self.to_string(),
                "type": error_type,
                "code": code,
            }
        }));

        (status, body).into_response()
    }
}
