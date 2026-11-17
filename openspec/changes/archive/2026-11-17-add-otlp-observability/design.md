# Design: add-otlp-observability

## Contexto

El gateway hoy: `metrics.rs` con atómicos lock-free pre-registrados y `render()` a texto Prometheus; `tracing` para logs; `/metrics` servido por axum. OpenObserve es OTLP-native y acepta métricas, logs y trazas por HTTP/OTLP con token de ingesta.

## Decisión 1 — OTel como fuente única de métricas

Se sustituyen los atómicos por instrumentos OTel (`Meter`/`Counter`/`Gauge`/`Histogram`) creados una vez a partir de la config (cardinalidad acotada, igual que hoy). Un único `MeterProvider` global lleva **dos readers**:

- `PrometheusExporter`: sirve `GET /metrics` con el contrato actual (`content-type: text/plain; version=0.0.4`, series `llm_gateway_*`). Siempre activo.
- `PeriodicReader` con exportador OTLP: solo se añade si `signals.metrics = true`.

Ventajas: una sola verdad, sin doble registro ni deriva. Alternativa descartada (atómicos + conversión a OTLP en un exportador propio): duplica la fuente y añade un encoder a mano.

Nota de nombres: OTel añade el sufijo `_total` a los counters en el texto Prometheus; los instrumentos se nombran sin `_total` (`llm_gateway_requests`) para que la salida conserve la serie `llm_gateway_requests_total`. Los histogramas exponen `_bucket`/`_sum`/`_count`, igual que hoy.

## Decisión 2 — Providers por señal y activación

- `MeterProvider`: siempre (para `/metrics`). Reader OTLP condicional.
- `LoggerProvider`: `opentelemetry-appender-tracing` como layer `tracing`, solo si `signals.logs = true`.
- `TracerProvider`: layer `tracing-opentelemetry` + `tracing-opentelemetry::OpenTelemetryLayer`, solo si `signals.traces = true`. Propagador W3C (`TraceContext`) instalado siempre que haya trazas.

Si el bloque `observability` está ausente o una señal está a `false`, esa señal no se inicializa: **cero cambio de comportamiento** y cero exportadores.

## Decisión 3 — Transporte

OTLP/HTTP (protobuf) con `opentelemetry-otlp` usando `reqwest` (ya presente, `rustls-tls`), para no arrastrar `tonic`/`grpc` salvo que `protocol: grpc` lo pida. Rutas: `{endpoint}/api/{organization}/v1/{metrics|logs|traces}`.

## Decisión 4 — Credenciales

El token se lee de la variable de entorno nombrada en `token_env` y se inyecta como cabecera `Authorization: Basic <token>` en el cliente OTLP. Si `token_env` está definido y la variable no existe, `config::load` falla.

## Decisión 5 — Cierre ordenado

`TelemetryGuard` (RAII) hace `force_flush` + `shutdown` de los providers en el cierre. `main.rs` llama a `guard.flush()` tras el graceful shutdown, antes de abortar los handles de warmup.

## Riesgos

- Refactor de `metrics.rs` (537 líneas con tests): mitigado conservando la fachada `record_*` y actualizando tests de `render()` al orden/formato del `PrometheusExporter` de OTel.
- Compatibilidad de versiones OTel: fijar versiones coherentes entre `opentelemetry`, `opentelemetry_sdk`, `opentelemetry-otlp`, `tracing-opentelemetry` y `opentelemetry-appender-tracing`.