# Design: add-toolcall-failover

## Context

Ver `proposal.md — Why`. Estado actual relevante: `route_with_failover` decide el failover **antes del primer byte** con la respuesta sin consumir el cuerpo; solo usa el `status`. En no-stream, tras un `2xx` bufferea el cuerpo (`full_response`) y lo devuelve. La spec vigente prohíbe interpretar el cuerpo.

## Goals / Non-Goals

- **Goals**: detectar el «2xx sin tool_calls» cuando la petición lo exige; reutilizar la cadena de failover; observabilidad del nuevo desenlace.
- **Non-Goals**: streaming; reescribir `tool_choice`; reintentar el mismo backend.

## Decisions

### Decisión: activar solo si `tool_choice` lo exige
Con `auto` el modelo puede legítimamente responder con texto; fallar ahí provocaría bucles y falsos positivos. Se exige `required`, `any` u objeto con función. Se comprueba una vez por petición (`request_requires_tool_call`) antes del bucle.

### Decisión: solo non-stream
En streaming los `tool_calls` llegan como deltas SSE; detectar su ausencia obligaría a bufferizar y romper el passthrough. Se limita a no-stream: la condición se ignora si `streaming == true`.

### Decisión: tratar el caso como intento reintentable, no como error
El `2xx` sin tool call se clasifica con `AttemptResult::NoToolCall`, se registra y se continúa el bucle, por el mismo camino que `RetryableStatus`. Se evita devolver al cliente una respuesta que incumple el contrato.

### Decisión: distinguir el agotamiento
Nueva variante `AppError::NoToolCallFromAllBackends` (estado `502`, código `no_tool_call`) frente a `AllBackendsFailed`. Da un mensaje distinguible sin romper el formato OpenAI. Se mantiene `502` por coherencia con el resto del failover.

### Decisión: inspección conservadora
Solo se marca fallo si el cuerpo parsea como JSON y se confirma `choices[*].message` sin `tool_calls`. Si no parsea o la forma es inesperada, se devuelve como éxito (no se penaliza una respuesta rara). La decisión se documenta en la spec (MODIFIED del requisito de passthrough).

## Risks / Trade-offs

- [Cuerpos grandes: parsear JSON en no-stream cuesta memoria/CPU] → ya se bufferiza el cuerpo entero; el parseo añade un coste acotado y solo cuando `tool_choice` lo exige.
- [Falsos positivos por `tool_choice` mal construido por el cliente] → se exige que `tools` sea lista no vacía; `auto`/`none`/ausente quedan exentos.
- [Latencia añadida por failover ante modelos que no llaman tools] → es el comportamiento deseado.

## Migration Plan

- Cambio aditivo y compatible: sin cambios de YAML; solo se altera la decisión de failover en el subconjunto `tool_choice` exigente no-stream.
- Rollback: revertir el commit; sin migración de datos.

## Open Questions

- Ninguna.