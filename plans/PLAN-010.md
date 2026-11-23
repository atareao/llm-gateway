# PLAN-010 — Gate de cuota free de OpenRouter y respaldo de pago barato

- **Issue**: #37 (atareao/llm-gateway)
- **Change OpenSpec**: `add-openrouter-free-quota-guard`
- **Estado**: pendiente de aprobación

## Objetivo

Dejar de quemar intentos free de OpenRouter cuando la cuota diaria (`remaining`) está agotada, y abaratar el respaldo de pago.

## Alcance

1. Gate global *free tier agotado hasta T* (reactivo por cabeceras `429` + `limit_source`).
2. Sondeo proactivo de `GET /api/v1/key` (`free_model_daily_requests.remaining`).
3. Métricas del gate y de la cuota.
4. `gateway.yaml.example` y docs con `z-ai/glm-5.3-flash` como respaldo de pago.
5. Actualización de la config de runtime `~/.config/opencode/gateway.yaml` (fuera del repo).

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
