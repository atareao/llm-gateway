# Tasks: add-otlp-observability

## 1. Configuración (RED → GREEN → REFACTOR)

- [x] 1.1 Tests (RED) en `src/config.rs`: parseo y validación del bloque `observability.otlp` (ausente, válido, endpoint vacío, interval_ms=0, timeout_ms=0, signals todas false, token_env inexistente). → cubiertos en `tests/config.rs` (`observability_absent_is_none`, `observability_otlp_is_parsed`, `observability_protocol_defaults_to_http`, `observability_endpoint_empty_is_rejected`, `observability_endpoint_blank_is_rejected`, `observability_organization_empty_is_rejected`, `observability_interval_zero_is_rejected`, `observability_timeout_zero_is_rejected`, `observability_all_signals_false_is_rejected`, `observability_token_env_missing_is_rejected`).
- [x] 1.2 (GREEN) Structs `Observability`, `Otlp`, `Protocol`, `Signals` y validación en `config::validate` (`validate_otlp`).
- [x] 1.3 Actualizar `gateway.yaml.example` con el bloque comentado.

## 2. Telemetría (RED → GREEN → REFACTOR)

- [x] 2.1 Tests (RED) en `src/telemetry.rs`: `init` es no-op sin bloque; con bloque construye los providers según `signals`. → tests unitarios (`signal_url_*`, `headers_*`, `interval_defaults_to_15_seconds`, `timeout_defaults_to_10_seconds`, `signals_any_requires_one_enabled`) y, a nivel de integración, `planned_exporters_follow_signals` y `no_observability_block_plans_no_exporters`.
- [x] 2.2 (GREEN) `TelemetryGuard` + `init`/`flush`; provider de métricas con `PrometheusExporter` y, si procede, `PeriodicReader(OTLP)`; logger y tracer condicionales; propagador W3C.
- [x] 2.3 (REFACTOR) Integrar en `main.rs` (init antes de servir, flush en el cierre).

## 3. Métricas sobre OTel (RED → GREEN → REFACTOR)

- [x] 3.1 Tests (RED) de la fachada `record_*` conservada y de `render()` con el contrato de nombres.
- [x] 3.2 (GREEN) Migrar `metrics.rs` a instrumentos OTel manteniendo la interfaz pública y la cardinalidad.
- [x] 3.3 (REFACTOR) Actualizar los tests existentes de `render()` al orden/formato del `PrometheusExporter`.

## 4. Logs (RED → GREEN → REFACTOR)

- [x] 4.1 Test (RED): con `signals.logs = true`, un evento de petición llega al receptor OTLP mock con sus campos (`otlp_happy_path_exports_metrics_logs_and_traces`).
- [x] 4.2 (GREEN) Layer `opentelemetry-appender-tracing` condicional.

## 5. Trazas (RED → GREEN → REFACTOR)

- [x] 5.1 Tests (RED): propagación de `traceparent` al backend y presencia de spans server/client en el receptor mock.
- [x] 5.2 (GREEN) Layer `tracing-opentelemetry`, spans en `proxy.rs`, propagador W3C instalado.

## 6. Integración y verificación

- [x] 6.1 Test de integración `tests/telemetry.rs`: receptor OTLP mock (axum) recibe métricas, logs y trazas.
- [x] 6.2 Test: sin bloque `observability`, `/metrics` rinde byte-compatible en nombres con hoy. → contrato de nombres cubierto en los tests de `metrics.rs` (`render_keeps_the_llm_gateway_series_names`) y ausencia de exportadores en `no_observability_block_plans_no_exporters`.
- [x] 6.3 `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check` y `openspec validate --strict` en verde. → verificado: `cargo fmt --check` OK, `cargo clippy --all-targets --all-features -- -D warnings` sin errores, `cargo test` en verde, `openspec validate add-otlp-observability --strict` → *Change is valid*.

## 7. Documentación y cierre

- [x] 7.1 `docs/openobserve-otlp.md` (qué es, ficheros, config, cómo habilitar, decisiones, follow-ups).
- [x] 7.2 `README.md`: sección de observabilidad con OTLP a OpenObserve.
- [ ] 7.3 Marcar tareas y archivar el change con `openspec archive add-otlp-observability`. → **PENDIENTE**: las tareas están marcadas, pero el *archive* no se ha ejecutado (queda a la espera de revisión/merge).
