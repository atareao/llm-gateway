# PLAN-011 — Registro de precios, selecciones y resultados en OpenObserve

- **Issue**: #39 (atareao/llm-gateway)
- **Change OpenSpec**: `add-observability-ledger`
- **Estado**: implementado (TDD, grupos 1-6) y auditado — PR #40 ready for review (base `development`). Consulta SQL en OpenObserve real: pendiente post-despliegue.
- **Depende de**: #37 (`add-openrouter-free-quota-guard`), que introduce la selección pegajosa y el respaldo dinámico cuyos eventos consume.

## Objetivo

Registrar de forma consultable, en OpenObserve, la evolución de precios del catálogo, las decisiones de selección de modelo y los resultados/coste por petición, sin base de datos local.

## Alcance

1. Parseo de `usage` (no-stream y stream con `stream_options.include_usage`).
2. Eventos `gateway_catalog`, `gateway_selection` y `gateway_requests` (log-on-change + denormalizado).
3. Métricas de tokens, coste, precio del modelo seleccionado, precio de la lista curada y contador de selecciones.
4. Documentación de retención y de consultas/dashboards.

## Tareas

Ver `openspec/changes/add-observability-ledger/tasks.md` (todas marcadas; 6.4 parcial: consulta OpenObserve pendiente post-despliegue).

## Verificación

- `cargo test` ✅ (369 tests), `cargo clippy -- -D warnings` ✅, `cargo fmt --check` ✅, `openspec validate --strict` ✅.
- `curl -s http://127.0.0.1:4010/metrics | rg 'tokens_total|cost_usd'` ✅ en E2E local con binario de la rama (puertos scratch 4091/4093 + mock); pendiente repetir contra la instancia desplegada tras el deploy.
- Consulta en OpenObserve: coste por modelo y día — **pendiente post-despliegue** (ver `docs/observability-ledger.md`).
