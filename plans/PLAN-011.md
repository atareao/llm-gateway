# PLAN-011 — Registro de precios, selecciones y resultados en OpenObserve

- **Issue**: #39 (atareao/llm-gateway)
- **Change OpenSpec**: `add-observability-ledger`
- **Estado**: pendiente de aprobación
- **Depende de**: #37 (`add-openrouter-free-quota-guard`), que introduce la selección pegajosa y el respaldo dinámico cuyos eventos consume.

## Objetivo

Registrar de forma consultable, en OpenObserve, la evolución de precios del catálogo, las decisiones de selección de modelo y los resultados/coste por petición, sin base de datos local.

## Alcance

1. Parseo de `usage` (no-stream y stream con `stream_options.include_usage`).
2. Eventos `gateway_catalog`, `gateway_selection` y `gateway_requests` (log-on-change + denormalizado).
3. Métricas de tokens, coste, precio del modelo seleccionado, precio de la lista curada y contador de selecciones.
4. Documentación de retención y de consultas/dashboards.

## Tareas

Ver `openspec/changes/add-observability-ledger/tasks.md`.

## Verificación

- `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check`.
- `curl -s http://127.0.0.1:4010/metrics | rg 'tokens_total|cost_usd'`.
- Consulta en OpenObserve: coste por modelo y día.
