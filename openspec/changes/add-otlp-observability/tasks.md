# Tasks: add-otlp-observability

## 1. Configuración (RED → GREEN → REFACTOR)

- [ ] 1.1 Tests (RED) en `src/config.rs`: parseo y validación del bloque `observability.otlp` (ausente, válido, endpoint vacío, interval_ms=0, timeout_ms=0, signals todas false, token_env inexistente).
- [ ] 1.2 (GREEN) Structs `Observability`, `Otlp`, `Protocol`, `Signals` y validación en `config::validate`.
- [ ] 1.3 Actualizar `gateway.yaml.example` con el bloque comentado.

## 2. Telemetría (RED → GREEN → REFACTOR)

- [ ] 2.1 Tests (RED) en `src/telemetry.rs`: `init` es no-op sin bloque; con bloque construye los providers según `signals`.
- [ ] 2.2 (GREEN) `TelemetryGuard` + `init`/`flush`; provider de métricas con `PrometheusExporter` y, si procede, `PeriodicReader(OTLP)`; logger y tracer condicionales; propagador W3C.
- [ ] 2.3 (REFACTOR) Integrar en `main.rs` (init antes de servir, flush en el cierre).

## 3. Métricas sobre OTel (RED → GREEN → REFACTOR)

- [ ] 3.1 Tests (RED) de la fachada `record_*` conservada y de `render()` con el contrato de nombres.
- [ ] 3.2 (GREEN) Migrar `metrics.rs` a instrumentos OTel manteniendo la interfaz pública y la cardinalidad.
- [ ] 3.3 (REFACTOR) Actualizar los tests existentes de `render()` al orden/formato del `PrometheusExporter`.

## 4. Logs (RED → GREEN → REFACTOR)

- [ ] 4.1 Test (RED): con `signals.logs = true`, un evento de petición llega al receptor OTLP mock con sus campos.
- [ ] 4.2 (GREEN) Layer `opentelemetry-appender-tracing` condicional.

## 5. Trazas (RED → GREEN → REFACTOR)

- [ ] 5.1 Tests (RED): propagación de `traceparent` al backend y presencia de spans server/client en el receptor mock.
- [ ] 5.2 (GREEN) Layer `tracing-opentelemetry`, spans en `proxy.rs`, propagador W3C instalado.

## 6. Integración y verificación

- [ ] 6.1 Test de integración `tests/telemetry.rs`: receptor OTLP mock (axum) recibe métricas, logs y trazas.
- [ ] 6.2 Test: sin bloque `observability`, `/metrics` rinde byte-compatible en nombres con hoy.
- [ ] 6.3 `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check` y `openspec validate --strict` en verde.

## 7. Documentación y cierre

- [ ] 7.1 `docs/openobserve-otlp.md` (qué es, ficheros, config, cómo habilitar, decisiones, follow-ups).
- [ ] 7.2 `README.md`: sección de observabilidad con OTLP a OpenObserve.
- [ ] 7.3 Marcar tareas y archivar el change con `openspec archive add-otlp-observability`.