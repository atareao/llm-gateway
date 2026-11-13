# Proposal: add-toolcall-failover

## Why

Si la petición exige el uso de herramientas (`tool_choice: required`/`any`/función concreta) pero un backend devuelve `200` con una respuesta de texto sin `tool_calls`, el gateway lo da por bueno hoy. Un backend local que ignora las tools «se come» la petición sin que intervenga el fallback. Tratar ese `200` como intento fallido y pasar al siguiente backend restablece el contrato de fiabilidad del gateway.

## What Changes

- Nueva condición de failover **post-respuesta** (solo non-stream): un `200` sin `tool_calls` cuenta como intento fallido **si y solo si** la petición exige tool call.
- La condición se activa únicamente cuando `tool_choice` es `required`, `any` o un objeto que nombra una función concreta, y `tools` es una lista no vacía. Con `auto`, `none` o ausente, el comportamiento actual queda intacto.
- En `stream: true` no se aplica: se mantiene el passthrough sin bufferizar.
- Nueva métrica de intento `no_tool_call` en `llm_gateway_attempts_total{result}`.
- Si la cadena se agota sin ninguna respuesta con `tool_calls`, se devuelve error estilo OpenAI con código `no_tool_call` (estado `502`, coherente con el resto del failover).
- El gateway pasa a **inspeccionar** el cuerpo de la respuesta (solo para esta decisión), sin alterar los bytes devueltos al cliente.

## Capabilities

- **New Capabilities**: (ninguna)
- **Modified Capabilities**: `gateway`

## Impact

- `src/proxy.rs`: `route_with_failover` inspecciona la respuesta no-stream antes de darla por buena; nuevos helpers `request_requires_tool_call` y `response_has_tool_call`.
- `src/error.rs`: nueva variante de error para «todas las respuestas sin tool call».
- `src/metrics.rs`: nueva variante `AttemptResult::NoToolCall` (arrays de intentos de 6 a 7).
- `openspec/specs/gateway/spec.md`: modifica el requisito de passthrough e incorpora el nuevo de failover.
- Sin dependencias nuevas. Sin cambios de config YAML.