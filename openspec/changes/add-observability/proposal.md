# Proposal: add-observability

## Why

El gateway funciona pero es una caja negra: no hay forma de responder "¿está actuando el failover?", "¿qué backend me falla?" o "¿cuánto tarda cada ruta?" sin leer logs a mano. Sin métricas agregadas no se puede alertar ni comparar backends.

## What Changes

- Nuevo módulo `src/metrics.rs` con un registro **lock-free** (atómicos) pre-registrado a partir de la configuración: una entrada por ruta y por backend, sin `HashMap` ni `Mutex` en la ruta caliente.
- Endpoint `GET /metrics` en formato de exposición de Prometheus (`text/plain; version=0.0.4`).
- Logs estructurados por petición (`request_id`, `route`, `streaming`, `outcome`, `attempts`, `latency_ms`) y por intento (`backend`, `result`, `latency_ms`).
- Cabecera `x-request-id`: se reutiliza la del cliente si viene y es válida; si no, se genera una.
- `GATEWAY_LOG_FORMAT=json` para emitir logs en JSON (por defecto, texto legible).

## Non-goals

- Trazas distribuidas (OpenTelemetry) y propagación de contexto entre servicios.
- Persistencia de métricas, dashboards o alertas: el gateway solo expone.
- Autenticación de `/metrics`: el gateway escucha en loopback por defecto.
- Métricas de negocio (tokens, coste): el gateway no interpreta el cuerpo.

## Impact

- `AppState` gana `metrics: Arc<Metrics>`.
- `route_with_failover` recibe los índices de ruta y backend para etiquetar sin buscar por nombre.
- Sin dependencias nuevas de runtime: solo se activa la feature `json` de `tracing-subscriber`.
- `README.md` documenta `/metrics`, las métricas y `GATEWAY_LOG_FORMAT`.

## Contracts

```rust
// src/metrics.rs
pub struct Metrics { /* ... */ }

impl Metrics {
    pub fn new(config: &GatewayConfig) -> Self;
    pub fn record_request(&self, route: usize, streaming: bool, outcome: Outcome, elapsed: Duration);
    pub fn record_attempt(&self, route: usize, backend: usize, result: AttemptResult, elapsed: Duration);
    pub fn record_failover(&self, route: usize);
    pub fn record_global(&self, outcome: GlobalOutcome);
    pub fn render(&self) -> String; // formato Prometheus
}

pub enum Outcome { Success, AllFailed }
pub enum GlobalOutcome { InvalidRequest, ModelNotFound }
pub enum AttemptResult { Success, RetryableStatus, TransportError, FirstByteTimeout, MissingApiKey, BodyError }
```

```text
GET /metrics -> 200 text/plain; version=0.0.4
```

Métricas expuestas:

| Métrica | Tipo | Etiquetas |
|---|---|---|
| `llm_gateway_requests_total` | counter | `route`, `streaming`, `outcome` |
| `llm_gateway_request_duration_seconds` | histogram | `route`, `streaming` |
| `llm_gateway_failover_total` | counter | `route` |
| `llm_gateway_backend_attempts_total` | counter | `route`, `backend`, `result` |
| `llm_gateway_backend_duration_seconds` | histogram | `route`, `backend` |
| `llm_gateway_errors_total` | counter | `kind` (`invalid_request`, `model_not_found`) |
