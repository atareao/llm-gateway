# Design: add-observability

## Contexto

`src/proxy.rs` ya emite `tracing::warn!` por intento fallido, pero:

- No hay correlación entre los logs de una misma petición.
- No hay agregación: saber cuántos failovers hubo exige contar líneas a mano.
- No hay latencias, así que no se puede comparar un backend local con uno remoto.

## Decisión 1: métricas propias con atómicos, no el crate `metrics`

El conjunto de series es **finito y conocido al arrancar**: rutas y backends salen de `gateway.yaml`. Por eso el registro se pre-registra en `Metrics::new(&config)` y cada serie es un `AtomicU64`; no hace falta `HashMap` ni `Mutex` en la ruta caliente.

Se descarta el crate `metrics` + `metrics-exporter-prometheus` porque añade dependencias y un registro global por hilo para un conjunto de series que ya conocemos de antemano. Se descarta también un `Mutex<HashMap<..>>` porque serializa todas las peticiones en un punto.

```rust
pub struct Metrics {
    routes: Vec<RouteMetrics>, // mismo orden que config.route
    errors: [AtomicU64; 2],    // invalid_request, model_not_found
}

struct RouteMetrics {
    name: String,
    requests: [AtomicU64; 2],  // success, all_failed
    failovers: AtomicU64,
    duration: Histogram,
    backends: Vec<BackendMetrics>, // mismo orden que route.backend
}

struct BackendMetrics {
    id: String,                // "<base_url>#<model>"
    attempts: [AtomicU64; 6],  // success, retryable_status, transport_error,
                               // first_byte_timeout, missing_api_key, body_error
    duration: Histogram,
}

struct Histogram {
    buckets: [AtomicU64; 11],  // 0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10, 30, 60, 120
    sum_micros: AtomicU64,
    count: AtomicU64,
}
```

`streaming` no multiplica las series: se guarda como etiqueta en `requests_total` y `request_duration_seconds`, pero el array de contadores es de 2×2 (outcome × streaming) dentro de `RouteMetrics`.

## Decisión 2: `/metrics` en el mismo listener

El gateway escucha en `127.0.0.1` por defecto, así que exponer `/metrics` en el mismo puerto no amplía la superficie de red. Un segundo listener obligaría a otra variable de entorno y a otro ciclo de vida.

## Decisión 3: `x-request-id` sin dependencias

Se reutiliza la cabecera `x-request-id` del cliente si es ASCII visible y mide entre 1 y 128 bytes; si no, se genera `req-<pid>-<contador>` con un `AtomicU64` global. No se añade `uuid`: el identificador solo necesita ser único dentro del proceso para correlacionar logs.

## Decisión 4: logs con `tracing` y formato conmutable

Se mantiene `tracing`. `main.rs` elige el formateador según `GATEWAY_LOG_FORMAT`:

- ausente o `text` → `tracing_subscriber::fmt()` (actual).
- `json` → `.json()` (requiere la feature `json` de `tracing-subscriber`).

Cada petición abre un span `request` con `request_id`, `route` y `streaming`; los eventos de intento y el evento final heredan el span, así que en JSON salen con esos campos.

## Consecuencias

- Cardinalidad acotada por la config: añadir una ruta o un backend añade series, pero no hay etiquetas de valores libres (nada de `model` pedido por el cliente ni de mensajes de error).
- Los contadores son `Relaxed`: solo se agregan, no se usan para sincronizar nada.
- `render()` recorre todas las series en cada scrape; con decenas de rutas es irrelevante.
- Los tests de integración pueden afirmar contadores exactos porque cada test levanta su propio `AppState` y su propio registro.
