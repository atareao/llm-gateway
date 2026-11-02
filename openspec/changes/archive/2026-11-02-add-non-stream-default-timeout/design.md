# Design

## Context

`src/proxy.rs::send` separa dos caminos:

- **Stream**: `tokio::time::timeout(budget, request.send())` con `budget = timeout_ms | DEFAULT_FIRST_BYTE_TIMEOUT` (5 s). Acota solo el arranque.
- **No-stream**: `request.timeout(timeout_ms)` **solo si** `timeout_ms` está definido. Si no, la petición queda **sin deadline** (salvo el `CONNECT_TIMEOUT` global de 4 s).

El fallo: un backend que acepta TCP pero no responde cuelga la petición no-stream y no dispara failover.

## Goals / Non-Goals

**Goals:**
- Acotar la petición no-stream ante un backend colgado, permitiendo el failover.
- Mantener `timeout_ms` opcional (sin **BREAKING**) y con prioridad cuando exista.
- Simetría conceptual con el presupuesto por defecto ya existente en streaming.

**Non-Goals:**
- Cambiar el valor o la semántica del default de streaming (`DEFAULT_FIRST_BYTE_TIMEOUT`).
- Hacer el default configurable por ruta o por variable de entorno (posible extensión futura).
- Failover a mitad de stream (imposible; ya documentado).
- Reintentos con backoff o circuit breaker.

## Decisions

### D1: Timeout **total** para no-stream, no "first byte"
La respuesta no-stream se bufferiza por completo antes de devolverse, así que no existe un "primer byte" observable por el cliente; el límite natural es el **total** de la petición. `reqwest::RequestBuilder::timeout` cubre todo el ciclo (conexión + cuerpo). La alternativa "first-byte guard" se descarta: no aporta y añade complejidad.

### D2: Valor por defecto = `DEFAULT_REQUEST_TIMEOUT` (120 s)
Holgado para generaciones locales largas pero suficiente para acotar un cuelgue. Coincide con el `timeout_ms` remoto (OpenRouter) usado hoy en producción. Es una **red de seguridad**: quien necesite más, declara `timeout_ms`.

### D3: Un vencimiento cuenta como fallo "antes del primer byte"
En no-stream no se emite nada al cliente hasta tener el cuerpo completo, por lo que el timeout siempre ocurre **antes del primer byte** y es elegible para failover, igual que un error de conexión.

### D4: No tocar el default de streaming
`DEFAULT_FIRST_BYTE_TIMEOUT` (5 s) se mantiene. Los backends locales en producción ya declaran `timeout_ms` explícito (30–60 s), así que el default no "muerde". Cambiarlo queda fuera de alcance.

## Risks / Trade-offs

- **Riesgo**: un default demasiado bajo abortaría generaciones legítimas largas. **Mitigación**: 120 s es holgado y `timeout_ms` explícito tiene prioridad.
- **Riesgo**: `request.timeout` es total (incluye la lectura del cuerpo); una respuesta muy lenta pero viva podría disparar el default. **Mitigación**: documentar que se suba `timeout_ms` para generaciones > 120 s.
- **Trade-off**: no exponemos el default por env (menor superficie). Reversible si surge la necesidad.
