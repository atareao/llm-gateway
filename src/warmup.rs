//! Keep-alive (warmup) de backends: mantiene el modelo residente enviando una
//! petición mínima OpenAI-compatible de forma periódica.
//!
//! Un backend con `warmup.interval_ms` recibe, al arrancar (primer tick
//! inmediato) y cada `interval_ms`, un `POST <base_url>/chat/completions` con
//! su `model` real, un único mensaje `user` y `max_tokens: 1`, **sin** `stream`.
//! Cada petición reinicia el temporizador de inactividad del backend (p. ej.
//! `keep_alive` de Ollama), evitando que descargue el modelo de memoria.
//!
//! Cada warmup se acota con el `warmup.timeout_ms` del backend o, si se omite,
//! con [`WARMUP_TIMEOUT`] (30 s). El ticker usa
//! [`tokio::time::MissedTickBehavior::Delay`]: tras un warmup más largo que el
//! intervalo no se disparan en ráfaga los ticks acumulados. La API key se
//! resuelve una sola vez al iniciar la tarea; si no resuelve, se registra un
//! único `failure` y el bucle continúa en `debug` sin peticiones HTTP.
//!
//! El warmup reutiliza el mismo camino que el tráfico normal para la URL, la
//! resolución de la API key y la sustitución de `model` (helpers de
//! [`crate::proxy`]), pero **no** registra intentos ni peticiones normales: solo
//! [`crate::metrics::Metrics::record_warmup`].

use std::time::Duration;

use serde_json::{Value, json};
use tokio::time::MissedTickBehavior;

use crate::config::Warmup;
use crate::proxy::backend_id;
use crate::proxy::{AppState, body_for_backend, chat_url, resolve_api_key};

/// Timeout total por defecto de cada petición de warmup (30 s).
///
/// Se usa cuando el backend no declara `warmup.timeout_ms`.
pub const WARMUP_TIMEOUT: Duration = Duration::from_secs(30);

/// Timeout efectivo de un warmup: el `timeout_ms` del backend o, si se omite,
/// [`WARMUP_TIMEOUT`].
fn warmup_timeout(warmup: &Warmup) -> Duration {
    warmup
        .timeout_ms
        .map(Duration::from_millis)
        .unwrap_or(WARMUP_TIMEOUT)
}

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

/// Ejecuta un único warmup contra `(route, backend)` con la API key ya resuelta
/// y registra el resultado.
///
/// El timeout de la petición es el `warmup.timeout_ms` del backend o, si se
/// omite, [`WARMUP_TIMEOUT`]. `api_key` es el valor de [`resolve_api_key`]:
/// `Some(None)` para un backend local (sin cabecera) y `Some(Some(_))` cuando
/// hay clave. Devuelve `true` si el backend respondió `2xx`.
///
/// El fallo de transporte (`reqwest::Error`) se registra como `warn` **con su
/// causa**; el valor de la clave nunca aparece en los logs.
async fn warmup_request(
    state: &AppState,
    route: usize,
    backend: usize,
    api_key: &Option<String>,
) -> bool {
    let Some(backend_cfg) = state
        .config
        .route
        .get(route)
        .and_then(|route| route.backend.get(backend))
    else {
        return false;
    };

    let timeout = backend_cfg
        .warmup
        .as_ref()
        .map(warmup_timeout)
        .unwrap_or(WARMUP_TIMEOUT);

    let payload = body_for_backend(&warmup_body(&backend_cfg.model), backend_cfg);
    let mut request = state
        .client
        .post(chat_url(&backend_cfg.base_url))
        .json(&payload)
        .timeout(timeout);
    if let Some(key) = api_key {
        request = request.bearer_auth(key);
    }

    let response = request.send().await;
    let ok = matches!(&response, Ok(response) if response.status().is_success());
    state.metrics.record_warmup(route, backend, ok);

    match &response {
        Ok(response) if ok => {
            tracing::debug!(backend = %backend_id(backend_cfg), "warmup completado");
        }
        Ok(response) => {
            tracing::warn!(
                backend = %backend_id(backend_cfg),
                status = %response.status(),
                result = "failure",
                "warmup fallido"
            );
        }
        Err(err) => {
            tracing::warn!(
                backend = %backend_id(backend_cfg),
                error = %err,
                result = "failure",
                "warmup fallido"
            );
        }
    }

    ok
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

    warmup_request(state, route, backend, &api_key).await
}

/// Lanza una tarea de fondo por cada backend con `warmup` habilitado.
///
/// Cada tarea ejecuta un bucle con `tokio::time::interval`: el primer tick es
/// inmediato (warmup de arranque) y después repite cada `interval_ms`, con
/// [`MissedTickBehavior::Delay`] —tras un warmup más largo que el intervalo, el
/// siguiente tick se programa `interval_ms` después de terminar el actual, sin
/// disparar en ráfaga los ticks acumulados—. Un warmup fallido nunca aborta el
/// bucle.
///
/// La API key se resuelve **una sola vez** al iniciar la tarea (no en cada
/// tick). Si no resuelve, se registra **un único** `failure` y un `debug` por
/// tick, sin emitir peticiones HTTP.
///
/// Devuelve un `JoinHandle` por backend con `warmup`; el llamante decide qué
/// hacer con ellos. `main.rs` los conserva y los aborta en el cierre ordenado
/// (SIGINT/SIGTERM).
///
/// # Notas
///
/// Un `warmup.interval_ms` de `0` se ignora (`continue`): `tokio::time::interval`
/// *paniquea* con un periodo cero, y [`crate::config::load`] ya lo rechaza, pero
/// [`AppState::new`] es público y no valida.
pub fn spawn_warmup(state: AppState) -> Vec<tokio::task::JoinHandle<()>> {
    let mut handles = Vec::new();

    for (route_idx, route) in state.config.route.iter().enumerate() {
        for (backend_idx, backend) in route.backend.iter().enumerate() {
            let Some(warmup) = backend.warmup.as_ref() else {
                continue;
            };

            let interval = Duration::from_millis(warmup.interval_ms);
            if interval.is_zero() {
                continue;
            }

            let api_key = resolve_api_key(backend);
            let state = state.clone();
            handles.push(tokio::spawn(async move {
                let mut ticker = tokio::time::interval(interval);
                ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

                match api_key {
                    Some(key) => loop {
                        ticker.tick().await;
                        warmup_request(&state, route_idx, backend_idx, &key).await;
                    },
                    None => {
                        // Sin clave: un solo `failure` (el `warn` ya lo emitió
                        // `resolve_api_key` al arrancar la tarea) y traza
                        // `debug` por tick, sin ninguna petición HTTP.
                        state.metrics.record_warmup(route_idx, backend_idx, false);
                        loop {
                            ticker.tick().await;
                            let backend = &state.config.route[route_idx].backend[backend_idx];
                            tracing::debug!(
                                backend = %backend_id(backend),
                                result = "failure",
                                "warmup omitido: api_key_env sin resolver"
                            );
                        }
                    }
                }
            }));
        }
    }

    handles
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{WARMUP_TIMEOUT, warmup_timeout};
    use crate::config::Warmup;

    /// El timeout efectivo es `WARMUP_TIMEOUT` (30 s) cuando se omite
    /// `timeout_ms`; si está presente, se usa tal cual.
    #[test]
    fn warmup_timeout_uses_default_when_none() {
        let without = Warmup {
            interval_ms: 240_000,
            timeout_ms: None,
        };
        assert_eq!(warmup_timeout(&without), WARMUP_TIMEOUT);
        assert_eq!(warmup_timeout(&without), Duration::from_secs(30));

        let explicit = Warmup {
            interval_ms: 60_000,
            timeout_ms: Some(300_000),
        };
        assert_eq!(warmup_timeout(&explicit), Duration::from_millis(300_000));
    }
}
