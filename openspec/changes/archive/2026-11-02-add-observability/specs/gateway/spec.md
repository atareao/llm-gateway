## ADDED Requirements

### Requirement: Endpoint de métricas Prometheus

El gateway SHALL exponer `GET /metrics` devolviendo `200` con `content-type: text/plain; version=0.0.4; charset=utf-8` y el cuerpo en formato de exposición de Prometheus (`# HELP`, `# TYPE` y líneas `nombre{etiquetas} valor`).

El endpoint SHALL ser de solo lectura y SHALL NOT requerir autenticación.

**Scenarios:**

#### Scenario: Scrape vacío
- **GIVEN** un gateway recién arrancado sin peticiones
- **WHEN** se hace `GET /metrics`
- **THEN** responde `200` con `text/plain; version=0.0.4`
- **AND** el cuerpo contiene `# TYPE llm_gateway_requests_total counter`

#### Scenario: Etiquetas escapadas
- **GIVEN** una ruta cuyo nombre contiene `"` o `\`
- **WHEN** se renderizan las métricas
- **THEN** el valor de la etiqueta se escapa con `\"` y `\\`

### Requirement: Métricas de peticiones, failover y backends

El gateway SHALL registrar, por cada petición a `/v1/chat/completions`:

- `llm_gateway_requests_total{route,streaming,outcome}` con `outcome` en `success` o `all_failed`.
- `llm_gateway_request_duration_seconds{route,streaming}` como histograma con los buckets `0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10, 30, 60, 120` y `+Inf`.
- `llm_gateway_failover_total{route}` incrementado **una vez** por petición que necesitó más de un intento.

Y por cada intento contra un backend:

- `llm_gateway_backend_attempts_total{route,backend,result}` con `result` en `success`, `retryable_status`, `transport_error`, `first_byte_timeout`, `missing_api_key` o `body_error`.
- `llm_gateway_backend_duration_seconds{route,backend}` como histograma con los mismos buckets.

Las peticiones rechazadas antes de resolver ruta SHALL incrementar `llm_gateway_errors_total{kind}` con `kind` en `invalid_request` o `model_not_found`.

**Scenarios:**

#### Scenario: Petición con éxito
- **GIVEN** una ruta con un backend que responde `200`
- **WHEN** llega una petición no-stream
- **THEN** `llm_gateway_requests_total{route="mech-writer",streaming="false",outcome="success"}` vale `1`
- **AND** `llm_gateway_backend_attempts_total{route="mech-writer",backend="<id>",result="success"}` vale `1`
- **AND** `llm_gateway_failover_total{route="mech-writer"}` vale `0`

#### Scenario: Failover por backend caído
- **GIVEN** una ruta con primario caído y fallback OK
- **WHEN** llega una petición
- **THEN** `llm_gateway_failover_total{route="mech-writer"}` vale `1`
- **AND** el primario acumula `result="transport_error"`
- **AND** el fallback acumula `result="success"`

#### Scenario: Failover por status reintentable
- **GIVEN** un primario que responde `503` y un fallback que responde `200`
- **WHEN** llega una petición
- **THEN** el primario acumula `result="retryable_status"`
- **AND** la respuesta es `200`

#### Scenario: Todos los backends fallan
- **GIVEN** una ruta con todos los backends caídos
- **WHEN** llega una petición
- **THEN** responde `502`
- **AND** `llm_gateway_requests_total{...,outcome="all_failed"}` vale `1`

#### Scenario: Modelo desconocido
- **GIVEN** una petición con `model: "no-existe"`
- **WHEN** se atiende
- **THEN** responde `404`
- **AND** `llm_gateway_errors_total{kind="model_not_found"}` vale `1`

#### Scenario: `api_key_env` sin resolver
- **GIVEN** un backend con `api_key_env` apuntando a una variable no definida
- **WHEN** se intenta usar
- **THEN** acumula `result="missing_api_key"` y se pasa al siguiente backend

### Requirement: Logs estructurados correlacionados

Cada petición a `/v1/chat/completions` SHALL abrir un span `request` con los campos `request_id`, `route` y `streaming`, y SHALL emitir al terminar un evento con `outcome`, `attempts` y `latency_ms`.

Cada intento contra un backend SHALL emitir un evento con `backend`, `result` y `latency_ms`; los fallos SHALL ser `warn` y los éxitos `debug`.

El gateway SHALL NOT registrar el valor de ninguna API key.

**Scenarios:**

#### Scenario: Correlación por request_id
- **GIVEN** una petición con `x-request-id: abc123`
- **WHEN** se atiende
- **THEN** todos los eventos de esa petición llevan `request_id="abc123"`

#### Scenario: Formato JSON
- **GIVEN** `GATEWAY_LOG_FORMAT=json`
- **WHEN** arranca el gateway
- **THEN** los logs se emiten como objetos JSON de una línea

### Requirement: Cabecera `x-request-id`

El gateway SHALL devolver `x-request-id` en todas las respuestas de `/v1/chat/completions`.

Si la petición trae `x-request-id` con un valor ASCII visible de 1 a 128 bytes, SHALL reutilizarlo; en caso contrario SHALL generar uno con el formato `req-<pid>-<contador>`.

**Scenarios:**

#### Scenario: Reutiliza el identificador del cliente
- **GIVEN** una petición con `x-request-id: cliente-42`
- **WHEN** se atiende
- **THEN** la respuesta trae `x-request-id: cliente-42`

#### Scenario: Genera uno si falta
- **GIVEN** una petición sin `x-request-id`
- **WHEN** se atiende
- **THEN** la respuesta trae un `x-request-id` con prefijo `req-`

#### Scenario: Valor inválido
- **GIVEN** una petición con `x-request-id` de 200 bytes
- **WHEN** se atiende
- **THEN** la respuesta trae un identificador generado, no el del cliente
