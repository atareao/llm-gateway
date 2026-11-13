# Tasks

## 1. RED — tests que fallan

- [x] 1.1 `tests/failover.rs`: primario responde `200` sin `tool_calls` y fallback con `tool_calls`, `tool_choice: "required"` → responde el fallback y `x-gateway-backend` lo identifica. Verificación: el test pasa.
- [x] 1.2 `tests/failover.rs`: `tool_choice: "auto"` con primario `200` sin `tool_calls` → NO se contacta el fallback (contador del fallback = 0). Verificación: el test pasa.
- [x] 1.3 `tests/failover.rs`: `tool_choice` ausente → NO se contacta el fallback. Verificación: el test pasa.
- [x] 1.4 `tests/failover.rs`: `tool_choice` objeto con función → failover al fallback. Verificación: el test pasa.
- [x] 1.5 `tests/failover.rs`: todos los backends `200` sin `tool_calls` → `502` con código `no_tool_call`. Verificación: el test pasa.
- [x] 1.6 `tests/failover.rs`: primario `200` **con** `tool_calls` y `tool_choice: "required"` → NO failover. Verificación: el test pasa.
- [x] 1.7 `tests/stream.rs`: `stream: true` con `tool_choice: "required"` → sin inspección ni failover. Verificación: el test pasa.
- [x] 1.8 `tests/metrics.rs` (o equivalente): un intento `no_tool_call` incrementa `llm_gateway_attempts_total{result="no_tool_call"}`. Verificación: el test pasa.
- [x] 1.9 Ejecutar `cargo test` y confirmar que 1.1–1.8 fallan y el resto sigue verde. Verificación: salida de `cargo test`.

## 2. GREEN — implementación mínima

- [x] 2.1 `src/proxy.rs`: `request_requires_tool_call(&Value) -> bool` (tools no vacío + tool_choice exigente). Verificación: cubierto por 1.1–1.6.
- [x] 2.2 `src/proxy.rs`: `response_has_tool_call(&[u8]) -> bool` (parsea JSON; `true` si algún `choices[*].message.tool_calls` no vacío; conservador si no parsea). Verificación: cubierto por 1.1, 1.6.
- [x] 2.3 `src/proxy.rs`: en `route_with_failover`, rama no-stream, tras `full_response`: si la petición exige tool call y el cuerpo no la trae → `AttemptResult::NoToolCall`, log `warn`, `continue`. Verificación: 1.1–1.4.
- [x] 2.4 `src/metrics.rs`: variante `AttemptResult::NoToolCall`, `label() = "no_tool_call"`, `index() = 6`; `ATTEMPT_RESULTS` y `attempts` a 7. Verificación: 1.8.
- [x] 2.5 `src/error.rs`: variante `NoToolCallFromAllBackends { model, attempts }` → `502`, código `no_tool_call`. Verificación: 1.5.
- [x] 2.6 `src/proxy.rs`: propagar si el fallo fue por no-tool-call para devolver `NoToolCallFromAllBackends` en lugar de `AllBackendsFailed`. Verificación: 1.5.
- [x] 2.7 `cargo test` en verde y `cargo check` sin errores. Verificación: salida de los comandos.

## 3. REFACTOR

- [x] 3.1 `cargo fmt --all -- --check`. Verificación: sin diferencias.
- [x] 3.2 `cargo clippy --all-targets -- -D warnings` sin warnings. Verificación: exit 0.
- [x] 3.3 Documentar el comportamiento en `README.md` y `docs/toolcall-failover.md`. Verificación: ficheros creados/actualizados.

## 4. CIERRE

- [x] 4.1 `openspec validate add-toolcall-failover --strict` OK. Verificación: comando sin errores.
- [x] 4.2 Archivar el change: `openspec archive add-toolcall-failover`. Verificación: change movido a `openspec/changes/archive/`.