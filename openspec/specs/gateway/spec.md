# gateway Specification

## Purpose
Proxy local compatible con la API de OpenAI que enruta cada petición a una cadena ordenada de backends y hace failover antes del primer token, permitiendo usar modelos locales sin perder robustez.

## Requirements

### Requirement: Configuración de rutas por YAML

El gateway SHALL configurarse con un fichero YAML (por defecto `gateway.yaml`, o la ruta indicada por la variable de entorno `GATEWAY_CONFIG`) que define una lista de rutas; cada ruta tiene un `name` (nombre lógico que pide el cliente) y una lista **ordenada** `backend` (primario primero, respaldos después). El parseo se hace con `serde-saphyr`.

- `pub struct GatewayConfig { pub route: Vec<Route> }`
- `pub struct Route { pub name: String, pub backend: Vec<Backend> }`
- `pub struct Backend { pub base_url: String, pub model: String, pub api_key_env: Option<String>, pub timeout_ms: Option<u64> }`

**Scenarios:**

#### Scenario: Config válida
- **GIVEN** un `gateway.yaml` con una ruta `mech-writer` y dos backends
- **WHEN** se carga la config
- **THEN** la ruta existe con 2 backends en orden

#### Scenario: Ruta sin backends
- **GIVEN** una ruta con `backend: []`
- **WHEN** se valida
- **THEN** error de configuración indicando el nombre de la ruta

#### Scenario: Fichero ausente
- **GIVEN** que `GATEWAY_CONFIG` apunta a un fichero inexistente
- **WHEN** arranca
- **THEN** el proceso aborta con un error claro

#### Scenario: YAML inválido
- **GIVEN** un `gateway.yaml` con sintaxis YAML incorrecta
- **WHEN** se carga
- **THEN** el proceso aborta con un error que incluye la línea y la columna del problema

### Requirement: Endpoint compatible con OpenAI

El gateway SHALL exponer `POST /v1/chat/completions`, que SHALL aceptar el mismo cuerpo que la API de OpenAI.

**Scenarios:**

#### Scenario: Modelo lógico conocido
- **GIVEN** una ruta `mech-writer`
- **WHEN** llega `POST /v1/chat/completions` con `{"model":"mech-writer", ...}`
- **THEN** se reenvía al backend primario con `model` sustituido por el suyo

#### Scenario: Modelo desconocido
- **GIVEN** que no existe la ruta `foo`
- **WHEN** llega `{"model":"foo"}`
- **THEN** responde `404` con error estilo OpenAI `model_not_found`

### Requirement: Paso transparente del cuerpo y de los tool_calls

El gateway SHALL NOT interpretar el cuerpo: SHALL reenviarlo tal cual **salvo `model`**.

**Scenarios:**

#### Scenario: Preserva campos desconocidos
- **GIVEN** un cuerpo con `tools`, `tool_choice`, `temperature`, `response_format` y un campo futuro `x_custom`
- **WHEN** se reenvía
- **THEN** el backend recibe exactamente esos campos
- **AND** solo `model` cambió

#### Scenario: Preserva tool_calls en la respuesta
- **GIVEN** que el backend responde con `tool_calls`
- **WHEN** el gateway devuelve la respuesta
- **THEN** el cliente recibe los `tool_calls` intactos

### Requirement: Streaming SSE

El gateway SHALL reenviar los chunks SSE del backend en streaming, sin bufferizar, terminando con `data: [DONE]`.

**Scenarios:**

#### Scenario: Passthrough de stream
- **GIVEN** `stream: true`
- **WHEN** el backend emite chunks SSE `data:`
- **THEN** el gateway los reenvía en streaming sin bufferizar
- **AND** termina con `data: [DONE]`

#### Scenario: Stream no solicitado
- **GIVEN** `stream` ausente o `false`
- **THEN** la respuesta se devuelve sin streaming

### Requirement: Failover antes del primer token

Ante fallo de conexión, timeout, `429` o `5xx` **antes de emitir el primer byte**, el gateway SHALL reintentar con el siguiente backend de la cadena.

**Scenarios:**

#### Scenario: Primario caído
- **GIVEN** `mech-writer` con el primario local caído y un fallback remoto
- **WHEN** llega la petición
- **THEN** intenta el primario, falla, y responde desde el fallback
- **AND** la respuesta incluye la cabecera `x-gateway-backend` con el fallback usado
  (formato `<base_url>#<model>`, p. ej. `http://localhost:11434/v1#qwen2.5:7b-instruct`)

#### Scenario: Primario OK
- **GIVEN** que el primario responde `200`
- **THEN** NO se usa el fallback
- **AND** `x-gateway-backend` indica el primario

#### Scenario: Todos los backends fallan
- **GIVEN** todos los backends caídos
- **THEN** responde `502` con un error estilo OpenAI

#### Scenario: Corte a mitad de stream
- **GIVEN** que el primario empieza a emitir y se corta
- **THEN** NO se reintenta (limitación documentada)
- **AND** se cierra el stream con error

### Requirement: Endpoints auxiliares

El gateway SHALL exponer `GET /v1/models` y `GET /healthz`.

**Scenarios:**

#### Scenario: Listado de modelos
- **GIVEN** la config cargada
- **WHEN** `GET /v1/models`
- **THEN** devuelve la lista de nombres lógicos de ruta

#### Scenario: Salud
- **WHEN** `GET /healthz`
- **THEN** responde `200` con `ok`

### Requirement: Secretos solo por entorno

El gateway SHALL resolver las claves de API exclusivamente desde variables de entorno, nunca desde el YAML.

**Scenarios:**

#### Scenario: Clave resuelta en tiempo de petición
- **GIVEN** un backend con `api_key_env = "OPENROUTER_API_KEY"`
- **WHEN** se reenvía
- **THEN** se añade `Authorization: Bearer <valor>`
- **AND** la clave NUNCA aparece en los logs

#### Scenario: Variable ausente
- **GIVEN** que `api_key_env` apunta a una variable no definida
- **THEN** ese backend se considera no disponible y se salta al siguiente

### Requirement: Presupuesto de timeout por defecto sin `timeout_ms`

Cuando un backend no declara `timeout_ms`, el gateway SHALL aplicar un presupuesto por defecto que acote la petición y permita disparar el failover. El presupuesto SHALL diferir según el modo:

- **No-stream**: un **timeout total** de la petición (`DEFAULT_REQUEST_TIMEOUT`), que abarca desde el establecimiento de la conexión hasta el último byte del cuerpo.
- **Stream**: un **time-to-first-byte** (`DEFAULT_FIRST_BYTE_TIMEOUT`), que acota únicamente el arranque; una vez recibidas las cabeceras, el stream fluye sin deadline.

Si el backend **sí** declara `timeout_ms`, ese valor SHALL tener prioridad sobre el presupuesto por defecto correspondiente. El vencimiento del presupuesto SHALL considerarse un fallo **antes del primer byte**, elegible para failover.

**Scenarios:**

#### Scenario: No-stream sin `timeout_ms` responde desde el fallback
- **GIVEN** una ruta cuyo primario no declara `timeout_ms` y acepta la conexión pero nunca responde
- **AND** un fallback que responde con normalidad
- **WHEN** llega una petición no-stream
- **THEN** al vencer `DEFAULT_REQUEST_TIMEOUT` el primario se considera fallido
- **AND** la respuesta la sirve el fallback
- **AND** `x-gateway-backend` identifica al fallback

#### Scenario: No-stream sin `timeout_ms` y sin fallback disponible
- **GIVEN** una ruta con un único backend sin `timeout_ms` que nunca responde
- **WHEN** llega una petición no-stream
- **THEN** al vencer `DEFAULT_REQUEST_TIMEOUT` el gateway responde `502` con error estilo OpenAI
- **AND** NO deja la petición colgada indefinidamente

#### Scenario: `timeout_ms` explícito tiene prioridad sobre el default
- **GIVEN** un backend que declara `timeout_ms` con un valor concreto
- **WHEN** llega una petición no-stream
- **THEN** se respeta ese `timeout_ms` y NO se aplica `DEFAULT_REQUEST_TIMEOUT`

#### Scenario: Stream sin `timeout_ms` acota solo el primer byte
- **GIVEN** un backend sin `timeout_ms` que tarda más que `DEFAULT_FIRST_BYTE_TIMEOUT` en emitir el primer byte
- **WHEN** llega una petición con `stream: true`
- **THEN** el gateway lo considera fallido antes del primer byte y prueba el siguiente backend de la cadena

### Requirement: Tests de integración deterministas

Los tests de integración SHALL simular un backend caído sin liberar el puerto efímero reservado, de forma que ningún otro test pueda enlazarlo y alterar el escenario.

El helper compartido `tests/common/mod.rs` SHALL exponer `pub async fn dead_backend() -> String`, que devuelve una `base_url` cuyo listener permanece enlazado y cierra cada conexión aceptada sin responder.

**Scenarios:**

#### Scenario: Puerto del backend caído no reutilizable
- **GIVEN** el `base_url` devuelto por `dead_backend()`
- **WHEN** otro componente intenta `TcpListener::bind` sobre esa misma dirección
- **THEN** el bind falla porque el puerto sigue ocupado

#### Scenario: Failover ante backend caído
- **GIVEN** una ruta con primario `dead_backend()` y fallback OK
- **WHEN** llega `POST /v1/chat/completions`
- **THEN** responde `200` con `x-gateway-backend` del fallback
- **AND** el test es estable en ejecuciones repetidas y en paralelo

#### Scenario: Sin duplicación del helper
- **GIVEN** los ficheros `tests/failover.rs`, `tests/stream.rs`, `tests/integration.rs` y `tests/secrets.rs`
- **WHEN** se compilan los tests
- **THEN** todos usan `common::dead_backend` y no queda ninguna copia local

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

### Requirement: Keep-alive de backends (warmup)

El gateway SHALL permitir declarar, por backend, un bloque opcional `warmup` con `interval_ms`. Cuando esté presente, el gateway SHALL enviar una petición mínima OpenAI-compatible a `POST <base_url>/chat/completions` de forma inmediata al arrancar y SHALL repetirla cada `interval_ms` milisegundos, para mantener el modelo residente en el backend. Si `warmup` está ausente, el gateway SHALL NOT enviar peticiones de warmup para ese backend.

**Scenarios:**

#### Scenario: Warmup deshabilitado no envía peticiones
- **GIVEN** un backend sin bloque `warmup`
- **WHEN** el gateway arranca y transcurre al menos un intervalo de observación
- **THEN** el backend NO recibe ninguna petición de warmup

#### Scenario: Warmup habilitado carga el modelo al arrancar
- **GIVEN** un backend con `warmup.interval_ms = 240000`
- **WHEN** el gateway arranca
- **THEN** el backend recibe de inmediato un `POST /v1/chat/completions` con su `model` real, un único mensaje `user` y `max_tokens: 1`
- **AND** el cuerpo NO incluye `stream: true`

#### Scenario: Heartbeat periódico
- **GIVEN** un backend con `warmup.interval_ms = 50`
- **WHEN** transcurren ~200 ms desde el arranque
- **THEN** el backend ha recibido al menos dos peticiones de warmup

#### Scenario: Un warmup fallido no interrumpe el keep-alive
- **GIVEN** un backend con `warmup` cuyo backend está caído
- **WHEN** el warmup falla
- **THEN** el gateway lo registra como `failure` y sigue repitiendo en el siguiente tick
- **AND** las peticiones normales de otros backends no se ven afectadas

### Requirement: Validación de la configuración de warmup

El gateway SHALL rechazar, al cargar la configuración, un bloque `warmup` cuyo `interval_ms` sea `0`, con un error que nombre la ruta y el backend afectados.

**Scenarios:**

#### Scenario: interval_ms cero
- **GIVEN** un `gateway.yaml` con un backend cuyo `warmup.interval_ms` es `0`
- **WHEN** se carga la config
- **THEN** el proceso falla con un error de configuración que identifica la ruta y el backend

#### Scenario: interval_ms válido
- **GIVEN** un `gateway.yaml` con `warmup.interval_ms = 240000`
- **WHEN** se carga la config
- **THEN** la config es válida y ese backend conserva su `warmup`

### Requirement: Métricas de warmup

El gateway SHALL exponer, para cada backend con `warmup` habilitado, un gauge `llm_gateway_backend_warm{route,backend}` que valga `1` si el último warmup respondió `2xx` y `0` en caso contrario, y un counter `llm_gateway_warmup_total{route,backend,result}` con `result` en `success` o `failure`. Para backends sin `warmup`, el gateway SHALL NOT emitir estas series.

**Scenarios:**

#### Scenario: Warmup con éxito
- **GIVEN** un backend con `warmup` que responde `200`
- **WHEN** se ejecuta un warmup
- **THEN** `llm_gateway_backend_warm{route="mech-writer",backend="<id>"}` vale `1`
- **AND** `llm_gateway_warmup_total{route="mech-writer",backend="<id>",result="success"}` vale `1`

#### Scenario: Warmup fallido
- **GIVEN** un backend con `warmup` caído
- **WHEN** se ejecuta un warmup
- **THEN** `llm_gateway_backend_warm{route="mech-writer",backend="<id>"}` vale `0`
- **AND** `llm_gateway_warmup_total{route="mech-writer",backend="<id>",result="failure"}` vale `1`

#### Scenario: Backend sin warmup no expone series
- **GIVEN** un backend sin `warmup`
- **WHEN** se hace `GET /metrics`
- **THEN** no aparece ninguna serie `llm_gateway_backend_warm` ni `llm_gateway_warmup_total` para ese backend

### Requirement: Autenticación del warmup

El warmup SHALL resolver la API key del backend por el mismo camino que el tráfico normal (`api_key_env`); el valor de la clave SHALL NOT aparecer en los logs. Si la variable de entorno no está definida, el warmup SHALL considerarse fallido y registrar `result="failure"`.

**Scenarios:**

#### Scenario: Cabecera de autorización presente
- **GIVEN** un backend con `warmup` y `api_key_env` resuelto
- **WHEN** se ejecuta un warmup
- **THEN** el backend recibe `Authorization: Bearer <valor>`

#### Scenario: Variable de entorno ausente
- **GIVEN** un backend con `warmup` y `api_key_env` apuntando a una variable no definida
- **WHEN** se ejecuta un warmup
- **THEN** el warmup se registra como `failure` y NO se emite ninguna petición HTTP
