//! Keep-alive (warmup) de backends: mantiene el modelo residente enviando una
//! petición mínima OpenAI-compatible de forma periódica.
//!
//! Un backend con `warmup.interval_ms` recibe, al arrancar (primer tick
//! inmediato) y cada `interval_ms`, un `POST <base_url>/chat/completions` con
//! su `model` real, un único mensaje `user` y `max_tokens: 1`, **sin** `stream`.
//! Cada petición reinicia el temporizador de inactividad del backend (p. ej.
//! `keep_alive` de Ollama), evitando que descargue el modelo de memoria.
//!
//! El warmup reutiliza el mismo camino que el tráfico normal para la URL, la
//! resolución de la API key y la sustitución de `model` (helpers de
//! [`crate::proxy`]), pero **no** registra intentos ni peticiones normales: solo
//! [`crate::metrics::Metrics::record_warmup`].

use std::time::Duration;

use serde_json::{Value, json};

use crate::proxy::backend_id;
use crate::proxy::{AppState, body_for_backend, chat_url, resolve_api_key};

/// Timeout total de cada petición de warmup.
pub const WARMUP_TIMEOUT: Duration = Duration::from_secs(30);

/// Construye el cuerpo mínimo OpenAI-compatible del warmup.
///
/// Incluye el `model` real, un único mensaje `user` con `"ping"` y
/// `max_tokens: 1`. No añade `stream` (equivale a no-stream).
fn warmup_body(model: &str) -> Value {
    json!({
        "model": model,
        "messages": [{"role": "user", "content": "ping"}],
        "max_tokens": 1,
    })
}

/// Ejecuta un único warmup contra `(route, backend)` y devuelve si respondió
/// `2xx`.
///
/// Resuelve la API key por el mismo camino que el tráfico normal. Si el backend
/// no existe, no tiene `warmup`, o su `api_key_env` no resuelve, devuelve
/// `false` sin emitir petición HTTP. Registra siempre el resultado en las
/// métricas de warmup (nunca el valor de la clave en los logs).
///
/// # Contract
///
/// `route` y `backend` son **índices posicionales** de `state.config.route` y de
/// su lista `backend`, respectivamente (el mismo par que consume
/// [`crate::metrics::Metrics::record_warmup`]). Si cualquiera de los dos queda
/// **fuera de rango**, devuelve `false` sin emitir petición HTTP ni tocar las
/// métricas.
pub async fn warmup_once(state: &AppState, route: usize, backend: usize) -> bool {
    let Some(backend_cfg) = state
        .config
        .route
        .get(route)
        .and_then(|route| route.backend.get(backend))
    else {
        return false;
    };

    if backend_cfg.warmup.is_none() {
        return false;
    }

    let Some(api_key) = resolve_api_key(backend_cfg) else {
        state.metrics.record_warmup(route, backend, false);
        // `resolve_api_key` ya emite el `warn` con el nombre de la variable y el
        // backend; aquí solo se baja a `debug` para no duplicar el aviso en cada
        // tick del keep-alive.
        tracing::debug!(
            backend = %backend_id(backend_cfg),
            result = "failure",
            "warmup fallido: api_key_env sin resolver"
        );
        return false;
    };

    let payload = body_for_backend(&warmup_body(&backend_cfg.model), backend_cfg);
    let mut request = state
        .client
        .post(chat_url(&backend_cfg.base_url))
        .json(&payload)
        .timeout(WARMUP_TIMEOUT);
    if let Some(key) = api_key {
        request = request.bearer_auth(key);
    }

    let ok = match request.send().await {
        Ok(response) => response.status().is_success(),
        Err(_) => false,
    };

    state.metrics.record_warmup(route, backend, ok);
    if ok {
        tracing::debug!(backend = %backend_id(backend_cfg), "warmup completado");
    } else {
        tracing::warn!(
            backend = %backend_id(backend_cfg),
            result = "failure",
            "warmup fallido"
        );
    }

    ok
}

/// Lanza una tarea de fondo por cada backend con `warmup` habilitado.
///
/// Cada tarea ejecuta un bucle con `tokio::time::interval`: el primer tick es
/// inmediato (warmup de arranque) y después repite cada `interval_ms`. Un
/// warmup fallido nunca aborta el bucle.
///
/// Devuelve un `JoinHandle` por backend con `warmup`. El llamante decide qué
/// hacer con ellos: `main.rs` los **descarta**, y Tokio deja entonces cada tarea
/// **detachada** hasta el fin del proceso (dropear un `JoinHandle` no cancela la
/// tarea). Para un apagado ordenado habría que guardarlos y hacer
/// `abort()`/`await` en el cierre.
///
/// # Precondición
///
/// `warmup.interval_ms > 0`, garantizado por [`crate::config::load`]:
/// `tokio::time::interval` *paniquea* con un periodo cero.
pub fn spawn_warmup(state: AppState) -> Vec<tokio::task::JoinHandle<()>> {
    let mut handles = Vec::new();

    for (route_idx, route) in state.config.route.iter().enumerate() {
        for (backend_idx, backend) in route.backend.iter().enumerate() {
            let Some(warmup) = backend.warmup.as_ref() else {
                continue;
            };

            let interval = Duration::from_millis(warmup.interval_ms);
            let state = state.clone();
            handles.push(tokio::spawn(async move {
                let mut ticker = tokio::time::interval(interval);
                loop {
                    ticker.tick().await;
                    warmup_once(&state, route_idx, backend_idx).await;
                }
            }));
        }
    }

    handles
}
