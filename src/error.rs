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
    /// El `model` pedido por el cliente no corresponde a ninguna ruta lógica.
    #[error("the model '{0}' does not exist")]
    ModelNotFound(String),

    /// El backend (primario) no está disponible o devolvió una respuesta ilegible.
    ///
    /// El failover entre backends llega en un lote posterior; por ahora solo se
    /// usa el primario y este error se devuelve al cliente.
    #[error("upstream backend error: {0}")]
    Upstream(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, error_type, code) = match &self {
            AppError::ModelNotFound(_) => (
                StatusCode::NOT_FOUND,
                "invalid_request_error",
                "model_not_found",
            ),
            AppError::Upstream(_) => (StatusCode::BAD_GATEWAY, "api_error", "upstream_error"),
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
