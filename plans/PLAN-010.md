# PLAN-010 — Gate de cuota free de OpenRouter y respaldo de pago dinámico

- **Issue**: #37 (atareao/llm-gateway)
- **Change OpenSpec**: `add-openrouter-free-quota-guard`
- **Estado**: implementado, auditado (APROBADO) y E2E verificada 2026-11-23 (PR #38)

## Objetivo

Dejar de quemar intentos free de OpenRouter cuando la cuota diaria (`remaining`) está agotada, y elegir dinámicamente el modelo de pago más barato dentro de una lista curada.

## Alcance

1. Gate global *free tier agotado hasta T* (reactivo por cabeceras `429` + `limit_source`).
2. Sondeo proactivo de `GET /api/v1/key` (`free_model_daily_requests.remaining`).
3. Métricas del gate y de la cuota.
4. Backend `kind: openrouter_paid` con lista curada de patrones (`discovery.allow`) y orden por precio (blend).
5. Selección pegajosa con histéresis (`reselect_interval_ms` 4 h, `min_price_delta` 0.20).
6. `gateway.yaml.example` y docs con el respaldo dinámico + `static` final opcional.
7. Actualización de la config de runtime `~/.config/opencode/gateway.yaml` (fuera del repo).

## Lista curada por defecto (`discovery.allow`)

- `deepseek/deepseek-v4.1-flash*`
- `z-ai/glm-5.*-flash`
- `qwen/qwen3.7-flash`
- `tencent/hy4-flash`
- `inclusionai/ling-3.1-flash`
- `nvidia/nemotron-3.5-lightning`
- `minimax/minimax-m3.*-flash`

Con el catálogo de 2026-11-23, el más barato de la lista es `z-ai/glm-5.3-flash` (blend $0.0484/M).

## Tareas

Ver `openspec/changes/add-openrouter-free-quota-guard/tasks.md`.

## Verificación

- `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check`.
- `curl -s http://127.0.0.1:4010/metrics | rg free_quota`.
- Inferencia real con `finish_reason: stop` y, con cuota agotada, cero intentos free.

## Contexto medido (2026-11-23)

- Cuota free: 1003/1000 usada, `remaining` 0.
- `retryable_status` free: architect 975, code 639, writer 175.
- Reparto: free 25,6 % vs pago 74,2 %; coste del día $1,85.