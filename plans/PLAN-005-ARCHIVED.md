# PLAN-005 — Failover cuando se envían tools y la respuesta no trae tool_calls

- **Issue:** [#21](https://github.com/atareao/llm-gateway/issues/21)
- **OpenSpec change:** `add-toolcall-failover`
- **Rama:** `feature/add-toolcall-failover`

## Objetivo

Que un `200` sin `tool_calls` deje de considerarse éxito cuando la petición exige una llamada a herramienta (`tool_choice: required`/`any`/función), disparando failover al siguiente backend. Solo non-stream; con `auto`/`none`/ausente no cambia nada; si se agota la cadena, error `502` con código `no_tool_call`.

## Alcance

- `src/proxy.rs`: helpers `request_requires_tool_call` y `response_has_tool_call`; nueva rama de failover no-stream.
- `src/metrics.rs`: `AttemptResult::NoToolCall` (arrays 6 → 7).
- `src/error.rs`: `NoToolCallFromAllBackends` (502, `no_tool_call`).
- Spec: `openspec/specs/gateway/spec.md` (MODIFIED passthrough + ADDED failover).
- Docs: `README.md`, `docs/toolcall-failover.md`.

## Fuera de alcance

- Detección de tool_calls en streaming.
- Reescritura de `tool_choice`.
- Reintento contra el mismo backend.

## Tareas

Derivadas de `openspec/changes/add-toolcall-failover/tasks.md` (RED → GREEN → REFACTOR → cierre).

## Criterio de cierre

- Tests 1.1–1.8 en verde; `cargo test`, `cargo clippy -D warnings` y `openspec validate --strict` OK.
- Change archivado y `docs/toolcall-failover.md` publicado.