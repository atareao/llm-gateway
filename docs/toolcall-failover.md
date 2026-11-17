# Failover ante respuesta sin tool_calls

## Qué es

Condición de failover **post-respuesta** que solo aplica en modo **no-stream**:
si la petición incluye `tools` como lista no vacía y `tool_choice` exige una
llamada a herramienta, una respuesta de éxito (`2xx`) cuyo cuerpo no contenga
`tool_calls` se considera **intento fallido** y el gateway prueba el siguiente
backend de la cadena, por el mismo camino que un status reintentable.

Motivación: algunos modelos/backends, ante un `tool_choice: "required"`,
responden con texto plano ignorando las herramientas. Antes eso se devolvía al
cliente como éxito, rompiendo el contrato que el cliente pidió. Con este cambio,
el gateway reintenta con el siguiente backend y, si ninguno cumple, responde un
error explícito en formato OpenAI.

Change OpenSpec: `openspec/changes/archive/2026-11-13-add-toolcall-failover/` (proposal, design,
spec delta, tasks).

## Ficheros implicados

- `src/proxy.rs` — `request_requires_tool_call`, `response_has_tool_call`,
  `route_with_failover` (rama no-stream) y `full_response` (devuelve también los
  bytes buffereados para inspeccionarlos sin recomputarlos).
- `src/metrics.rs` — variante `AttemptResult::NoToolCall` (`result="no_tool_call"`).
- `src/error.rs` — variante `AppError::NoToolCallFromAllBackends` → `502`,
  `type: "api_error"`, `code: "no_tool_call"`.
- `tests/failover.rs`, `tests/stream.rs`, `tests/metrics.rs`, `tests/endpoint.rs`,
  `tests/common/mod.rs` — escenarios y mocks.
- `README.md` — lista de valores de `result`.

## Cómo funciona

1. **Antes del bucle** se decide una sola vez por petición:
   `requires_tool_call = !streaming && request_requires_tool_call(body)`.
2. `request_requires_tool_call` exige `tools` como lista **no vacía** y un
   `tool_choice` estricto: la cadena `"required"`, `"any"` o un objeto que
   contenga la clave `function`.
3. En la rama no-stream, tras bufferear el cuerpo con `full_response`, si la
   respuesta es de **éxito** (`2xx`) y `!response_has_tool_call(&bytes)`:
   - se registra el intento como `AttemptResult::NoToolCall` y un `tracing::warn!`;
   - se continúa con el siguiente backend (`continue`).
4. Si la cadena se agota, el motivo decide el error: si hubo **al menos un**
   intento `NoToolCall`, se devuelve `AppError::NoToolCallFromAllBackends` (con
   **prioridad** sobre `AllBackendsFailed`), que responde `502` con
   `code: "no_tool_call"`.

`response_has_tool_call` es **conservador**: devuelve `true` (no penaliza) si el
cuerpo no parsea como JSON, si no trae `choices` (o están vacías) o si la forma
de alguna `choice`/`message` es inesperada. Solo devuelve `false` cuando
confirma la ausencia de `tool_calls` en todas las `choices`
(`message` sin `tool_calls` o con `tool_calls: []`).

Los bytes que recibe el cliente son **idénticos** a los del upstream (passthrough
exacto, incluido el `content-type`): la inspección trabaja sobre una copia barata
de los bytes ya buffereados y no re-serializa nada.

## Casos en que NO aplica

- `tool_choice: "auto"`, `"none"` o **ausente** (el modelo puede legítimamente
  responder con texto).
- `tools` **ausente** o como **lista vacía**.
- `tool_choice` como objeto **sin** clave `function`.
- `stream: true` (los `tool_calls` llegarían como deltas SSE; detectarlos
  obligaría a bufferizar y rompería el passthrough). En streaming no hay
  inspección ni failover por esta causa.
- Respuestas **no-2xx** (`3xx`/`4xx`): se devuelven tal cual, sin inspeccionar el
  cuerpo ni disparar el failover.

## Métricas

- `llm_gateway_backend_attempts_total{route,backend,result="no_tool_call"}` —
  cuenta los intentos descartados por este motivo.
- La petición que agota la cadena se contabiliza como `outcome="all_failed"` en
  `llm_gateway_requests_total`, e incrementa `llm_gateway_failover_total` cuando
  hubo más de un intento.

## Decisión de diseño

- **Solo con `tool_choice` estricto**: con `auto` el modelo puede responder texto
  sin incumplir nada; fallar ahí provocaría bucles y falsos positivos.
- **Solo no-stream**: en streaming no se bufferiza para no romper el passthrough.
- **Como intento reintentable, no como error**: el `2xx` sin `tool_calls` se
  registra y se continúa el bucle, evitando devolver al cliente una respuesta que
  incumple el contrato.
- **Agotamiento distinguible**: `NoToolCallFromAllBackends` (`502`,
  `no_tool_call`) frente a `AllBackendsFailed` (`502`, `upstream_error`).
- **Inspección conservadora**: ante formas inesperadas se devuelve como éxito
  para no penalizar respuestas raras.

## Follow-ups

- Valorar extender la condición a streaming (deltas SSE de `tool_calls`) si algún
  backend lo requiere; hoy queda fuera por el coste de bufferizar.
- Valorar un contador de promociones/failovers atribuido al motivo
  (`no_tool_call`) para diagnósticos más finos.
