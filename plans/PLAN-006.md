# PLAN-006 — Observabilidad OTLP a OpenObserve (métricas, logs y trazas)

- **Issue:** [#25](https://github.com/atareao/llm-gateway/issues/25)
- **OpenSpec change:** `add-otlp-observability`
- **Rama:** `feature/add-otlp-observability`

## Objetivo

Exportar métricas, logs y trazas del gateway por **OTLP** a OpenObserve, de forma config-driven y **deshabilitada por defecto**: sin el bloque `observability.otlp`, el comportamiento no cambia.

## Alcance

- `src/config.rs`: bloque opcional `observability.otlp` y su validación.
- `src/telemetry.rs` (nuevo): construcción de los `MeterProvider`/`LoggerProvider`/`TracerProvider` OTLP, propagador W3C, y `flush`/`shutdown`.
- `src/metrics.rs`: OTel como **fuente única**; el registro por atómicos pasa a instrumentos OTel y `/metrics` se sirve con `PrometheusExporter`.
- `src/main.rs`: inicialización de telemetría antes de servir y flush en el cierre ordenado.
- `src/proxy.rs`: spans del servidor HTTP y de los clientes `reqwest`, con propagación de contexto.
- `Cargo.toml`: `opentelemetry`, `opentelemetry_sdk`, `opentelemetry-otlp`, `opentelemetry-prometheus`, `tracing-opentelemetry`, `opentelemetry-appender-tracing`.
- `gateway.yaml.example`, `README.md`, `docs/openobserve-otlp.md`, `tests/telemetry.rs`.
- Spec: `openspec/changes/add-otlp-observability/specs/gateway/spec.md`.

## Fuera de alcance

- Prometheus remote_write.
- Renombrar las series `llm_gateway_*` (contrato observable).
- Eliminar el endpoint `/metrics`.
- Sampling avanzado, dashboards y alertas.

## Decisión de diseño

OTel es la fuente única de métricas. Un `MeterProvider` global con dos readers: `PrometheusExporter` (sirve `/metrics`) y `PeriodicReader(OTLP)` (solo si `signals.metrics = true`). Evita doble registro y deriva entre lo scrapeado y lo exportado. Ver `design.md` del change.

## Tareas

Derivadas de `openspec/changes/add-otlp-observability/tasks.md` (RED → GREEN → REFACTOR → cierre).

## Criterio de cierre

- Escenarios de la spec en verde; `cargo test`, `cargo clippy -D warnings`, `cargo fmt --check` y `openspec validate --strict` OK.
- Change archivado, `docs/openobserve-otlp.md` publicado y `README.md` actualizado.