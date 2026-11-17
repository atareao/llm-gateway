# Proposal: add-otlp-observability

## Why

OpenObserve es el destino de observabilidad actual y es OTLP-native. Hoy el gateway expone `/metrics` (pull, formato Prometheus) y logs por `tracing`, sin forma de enviarlos a OpenObserve. Se quiere una tubería única OTLP para métricas, logs y trazas, sin depender de Prometheus.

## What Changes

- Nuevo bloque opcional `observability.otlp` en `gateway.yaml` (endpoint, organización, protocolo, `token_env`, `service_name`, intervalos y flags por señal). Deshabilitado por defecto.
- Nuevo módulo `src/telemetry.rs`: construye los providers OTLP, instala el propagador W3C y expone `flush`/`shutdown`.
- `src/metrics.rs`: el registro de métricas pasa a instrumentos OTel (fuente única). `/metrics` se sigue sirviendo con el mismo contrato de nombres `llm_gateway_*`, ahora desde el `PrometheusExporter` de OTel.
- Exportación OTLP de métricas a `{endpoint}/api/{organization}/v1/metrics`.
- Exportación OT3 de logs (`tracing` → OTLP) a `{endpoint}/api/{organization}/v1/logs`.
- Exportación OTLP de trazas (spans HTTP server/client) a `{endpoint}/api/{organization}/v1/traces`, con propagación de contexto W3C.
- Apagado ordenado con flush de los providers.
- Credenciales de ingesta leídas de una variable de entorno (`token_env`); nunca en el YAML.

## Non-goals

- Prometheus remote_write.
- Cambiar o renombrar las series `llm_gateway_*`.
- Eliminar el endpoint `/metrics`.
- Sampling avanzado, dashboards o alertas.
- Métricas de negocio (tokens, coste): el gateway no interpreta el cuerpo.

## Impact

- `AppState` no cambia de forma; la telemetría se inicializa en `main.rs`.
- `metrics.rs` cambia de implementación interna (atómicos → instrumentos OTel); la interfaz `record_*` se conserva como fachada.
- Nuevas dependencias OTel en `Cargo.toml`.
- `README.md` y `docs/openobserve-otlp.md` documentan la configuración y el uso.

## Contracts

```text
POST {endpoint}/api/{organization}/v1/metrics   (OTLP/HTTP, protobuf)
POST {endpoint}/api/{organization}/v1/logs      (OTLP/HTTP, protobuf)
POST {endpoint}/api/{organization}/v1/traces    (OTLP/HTTP, protobuf)
Header: Authorization: Basic <token>   (desde la env nombrada en token_env)
Header: traceparent                    (propagación W3C)
```

```rust
// src/config.rs
pub struct GatewayConfig {
    pub route: Vec<Route>,
    pub observability: Option<Observability>, // nuevo
}

pub struct Observability { pub otlp: Option<Otlp> }

pub struct Otlp {
    pub endpoint: String,
    pub organization: String,
    pub protocol: Protocol,        // Http | Grpc, por defecto Http
    pub token_env: Option<String>,
    pub service_name: Option<String>,
    pub interval_ms: Option<u64>,
    pub timeout_ms: Option<u64>,
    pub signals: Signals,
}

pub struct Signals { pub metrics: bool, pub logs: bool, pub traces: bool }

// src/telemetry.rs
pub struct TelemetryGuard { /* ... */ }
pub fn init(config: &GatewayConfig) -> Result<TelemetryGuard>;
impl TelemetryGuard { pub fn flush(&self); }

// src/metrics.rs (interfaz pública conservada)
impl Metrics {
    pub fn new(config: &GatewayConfig) -> Self;
    pub fn record_request(&self, route: usize, streaming: bool, outcome: Outcome, elapsed: Duration);
    pub fn record_attempt(&self, route: usize, backend: usize, result: AttemptResult, elapsed: Duration);
    pub fn record_failover(&self, route: usize);
    pub fn record_global(&self, outcome: GlobalOutcome);
    pub fn record_warmup(&self, route: usize, backend: usize, ok: bool);
    pub fn render(&self) -> String; // contrato Prometheus preservado
}
```

Series exportadas (nombres sin cambios):

| Métrica (texto Prometheus) | Instrumento OTel | Tipo | Etiquetas |
|---|---|---|---|
| `llm_gateway_requests_total` | `llm_gateway_requests` | counter | `route`, `streaming`, `outcome` |
| `llm_gateway_request_duration_seconds` | `llm_gateway_request_duration_seconds` | histogram | `route`, `streaming` |
| `llm_gateway_failover_total` | `llm_gateway_failover` | counter | `route` |
| `llm_gateway_backend_attempts_total` | `llm_gateway_backend_attempts` | counter | `route`, `backend`, `result` |
| `llm_gateway_backend_duration_seconds` | `llm_gateway_backend_duration_seconds` | histogram | `route`, `backend` |
| `llm_gateway_errors_total` | `llm_gateway_errors` | counter | `kind` |
| `llm_gateway_backend_warm` | `llm_gateway_backend_warm` | gauge | `route`, `backend` |
| `llm_gateway_warmup_total` | `llm_gateway_warmup` | counter | `route`, `backend`, `result` |