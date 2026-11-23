# gateway Specification

## Purpose
Proxy local compatible con la API de OpenAI que enruta cada petición a una cadena ordenada de backends y hace failover antes del primer token, permitiendo usar modelos locales sin perder robustez.

## Requirements

### Requirement: Configuración de rutas por YAML

El gateway SHALL configurarse con un fichero YAML (por defecto `gateway.yaml`, o la ruta indicada por `GATEWAY_CONFIG`) que define una lista de rutas; cada ruta tiene un `name` y una lista ordenada `backend`. Cada `Backend` SHALL tener un `kind` opcional (`static` por defecto u `openrouter_free`). Un `Backend` con `kind: static` SHALL requerir `base_url` y `model`. Un `Backend` con `kind: openrouter_free` SHALL requerir `base_url` y `api_key_env`, SHALL aceptar un `profile` opcional (`architect` | `code` | `writer`, default `code`) y un bloque `discovery` opcional. El bloque `Discovery` SHALL aplicar defaults por campo: `refresh_interval_ms` (default `300000`) y `blacklist_cooldown_ms` (default `60000`); `min_context` y `overrides` SHALL ser opcionales. Un bloque `discovery` con solo `min_context` SHALL ser válido. Los valores explícitos de `refresh_interval_ms` o `blacklist_cooldown_ms` iguales a `0` SHALL ser error de configuración que indique la ruta y el backend.

- `pub struct Discovery { pub refresh_interval_ms: u64, pub blacklist_cooldown_ms: u64, pub min_context: Option<u32>, pub overrides: Option<ClassifyOverrides> }`
- `refresh_interval_ms` SHALL deserializarse con `#[serde(default = "default_refresh_interval_ms")]`.
- `blacklist_cooldown_ms` SHALL deserializarse con `#[serde(default = "default_blacklist_cooldown_ms")]`.

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

#### Scenario: Backend estático sin `kind`
- **GIVEN** un `gateway.yaml` con un backend sin `kind` y con `base_url` y `model`
- **WHEN** se carga la config
- **THEN** el backend se interpreta como `kind: static` y la ruta queda disponible

#### Scenario: Backend dinámico válido
- **GIVEN** un backend con `kind: openrouter_free`, `base_url`, `api_key_env` y `profile: code`
- **WHEN** se carga la config
- **THEN** el backend se acepta como dinámico con perfil `code`

#### Scenario: Backend estático sin `model`
- **GIVEN** un backend con `kind: static` sin `model`
- **WHEN** se valida
- **THEN** error de configuración indicando el nombre de la ruta y el backend

#### Scenario: Backend dinámico sin `api_key_env`
- **GIVEN** un backend con `kind: openrouter_free` sin `api_key_env`
- **WHEN** se valida
- **THEN** error de configuración indicando el backend afectado

#### Scenario: Perfil inválido
- **GIVEN** un backend con `kind: openrouter_free` y `profile: cualquiera`
- **WHEN** se valida
- **THEN** error de configuración listando los perfiles admitidos

#### Scenario: `discovery` con solo `min_context`
- **GIVEN** un backend `openrouter_free` con `discovery: { min_context: 32768 }`
- **WHEN** se carga la config
- **THEN** el bloque se acepta con `refresh_interval_ms` y `blacklist_cooldown_ms` en sus valores por defecto

#### Scenario: `discovery` vacío
- **GIVEN** un backend `openrouter_free` con `discovery: {}`
- **WHEN** se carga la config
- **THEN** el bloque se acepta con todos los campos en sus valores por defecto

#### Scenario: TTL cero explícito
- **GIVEN** un bloque `discovery` con `refresh_interval_ms: 0`
- **WHEN** se valida
- **THEN** error de configuración que indica ruta, backend y campo

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

El gateway SHALL reenviar el cuerpo tal cual, sustituyendo únicamente el campo `model` por el del backend de destino. El gateway SHALL NOT modificar ni eliminar ningún otro campo de la petición, incluidos `tools` y `tool_choice`. El gateway MAY inspeccionar el cuerpo de la **respuesta** para decidir el failover, pero SHALL devolver al cliente los bytes del upstream sin alterarlos.

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

#### Scenario: La inspección no altera la respuesta
- **GIVEN** una respuesta `200` cuyo cuerpo se inspecciona para decidir el failover
- **WHEN** el gateway la devuelve al cliente
- **THEN** el cuerpo es byte a byte el del upstream
- **AND** el `content-type` del upstream se preserva

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

El gateway SHALL servir este endpoint desde el mismo registro OTel que alimenta la exportación OTLP, de modo que exista una única fuente de verdad. El gateway SHALL conservar los nombres de serie `llm_gateway_*` como contrato observable.

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

#### Scenario: Mismo registro para scrape y export
- **GIVEN** una petición registrada
- **WHEN** se consulta `GET /metrics` y, a la vez, se exporta por OTLP
- **THEN** el valor de la serie coincide en ambos

#### Scenario: El endpoint sigue disponible sin observabilidad
- **GIVEN** un fichero de configuración sin el bloque `observability`
- **WHEN** se hace `GET /metrics`
- **THEN** responde `200` con el formato y los nombres `llm_gateway_*` de hoy

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

El gateway SHALL permitir declarar, por backend, un bloque opcional `warmup` con `interval_ms` y un `timeout_ms` opcional. Cuando esté presente, el gateway SHALL enviar una petición mínima OpenAI-compatible a `POST <base_url>/chat/completions` de forma inmediata al arrancar y SHALL repetirla cada `interval_ms` milisegundos, para mantener el modelo residente en el backend. Si `warmup` está ausente, el gateway SHALL NOT enviar peticiones de warmup para ese backend. Cada warmup SHALL acotarse con el `timeout_ms` del backend o, si se omite, con un valor por defecto de 30 s. Tras un warmup que dure más que `interval_ms`, el gateway SHALL NOT disparar los ticks acumulados en ráfaga: SHALL reanudar la cadencia desde el instante de finalización del warmup.

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

#### Scenario: Sin ráfaga tras un warmup lento
- **GIVEN** un backend con `warmup.interval_ms = 5000` cuyo warmup tarda más de 5 s (p. ej. cold load o timeout)
- **WHEN** el warmup por fin termina
- **THEN** el gateway NO dispara de inmediato una ráfaga de warmups por los ticks acumulados
- **AND** el siguiente warmup se ejecuta ~`interval_ms` después de terminar el anterior

#### Scenario: Timeout de warmup configurable
- **GIVEN** un backend con `warmup.interval_ms = 60000` y `warmup.timeout_ms = 300000`
- **WHEN** el cold load del modelo tarda entre 30 s y 300 s
- **THEN** el warmup de arranque espera hasta 300 s y se registra como `success`
- **AND** sin `timeout_ms`, ese mismo backend se acotaría al default de 30 s

### Requirement: Validación de la configuración de warmup

El gateway SHALL rechazar, al cargar la configuración, un bloque `warmup` cuyo `interval_ms` sea `0` o cuyo `timeout_ms`, si está presente, sea `0`, con un error que nombre la ruta y el backend afectados.

**Scenarios:**

#### Scenario: interval_ms cero
- **GIVEN** un `gateway.yaml` con un backend cuyo `warmup.interval_ms` es `0`
- **WHEN** se carga la config
- **THEN** el proceso falla con un error de configuración que identifica la ruta y el backend

#### Scenario: timeout_ms cero
- **GIVEN** un `gateway.yaml` con un backend cuyo `warmup.timeout_ms` es `0`
- **WHEN** se carga la config
- **THEN** el proceso falla con un error de configuración que identifica la ruta y el backend

#### Scenario: interval_ms válido
- **GIVEN** un `gateway.yaml` con `warmup.interval_ms = 240000`
- **WHEN** se carga la config
- **THEN** la config es válida y ese backend conserva su `warmup`

#### Scenario: timeout_ms válido y opcional
- **GIVEN** un `gateway.yaml` con dos backends: uno con `warmup.timeout_ms = 300000` y otro sin `timeout_ms`
- **WHEN** se carga la config
- **THEN** la config es válida; el primero conserva su `timeout_ms` y el segundo lo tiene a `None`

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

El warmup SHALL resolver la API key del backend por el mismo camino que el tráfico normal (`api_key_env`); el valor de la clave SHALL NOT aparecer en los logs. La resolución SHALL realizarse **una vez** al iniciar la tarea de keep-alive del backend, no en cada tick. Si la variable de entorno no está definida, el warmup SHALL registrarse como fallido y SHALL emitir **como máximo un** aviso `warn` por backend (no uno por tick), sin emitir ninguna petición HTTP.

**Scenarios:**

#### Scenario: Cabecera de autorización presente
- **GIVEN** un backend con `warmup` y `api_key_env` resuelto
- **WHEN** se ejecuta un warmup
- **THEN** el backend recibe `Authorization: Bearer <valor>`

#### Scenario: Variable de entorno ausente
- **GIVEN** un backend con `warmup` y `api_key_env` apuntando a una variable no definida
- **WHEN** se ejecuta un warmup
- **THEN** el warmup se registra como `failure` y NO se emite ninguna petición HTTP

#### Scenario: Aviso único por backend
- **GIVEN** un backend con `warmup` y `api_key_env` sin resolver
- **WHEN** transcurren varios ticks de keep-alive
- **THEN** el aviso `warn` de clave no resuelta se emite una sola vez para ese backend
- **AND** los ticks posteriores no repiten el `warn`

### Requirement: Diagnóstico de fallos de warmup

Cuando un warmup falla por un error de transporte (conexión rechazada, timeout de la petición o error de red), el gateway SHALL registrar la causa subyacente en el log de nivel `warn`, incluyendo el backend afectado, y SHALL NOT descartar el mensaje de error.

**Scenarios:**

#### Scenario: Fallo de transporte con causa
- **GIVEN** un backend con `warmup` que no acepta conexiones
- **WHEN** el warmup falla por error de transporte
- **THEN** el log incluye el backend afectado y la causa del error
- **AND** el resultado se registra como `failure`

#### Scenario: Timeout de la petición con causa
- **GIVEN** un backend con `warmup` que tarda más que el `timeout_ms` configurado
- **WHEN** la petición de warmup expira
- **THEN** el log identifica el backend y que el fallo se debió al timeout

### Requirement: Apagado ordenado de las tareas de warmup

El gateway SHALL conservar los handles de las tareas de keep-alive y, al recibir una señal de terminación (SIGINT o SIGTERM), SHALL abortarlas antes de finalizar el proceso, de modo que no queden tareas de fondo activas durante el cierre.

**Scenarios:**

#### Scenario: Abortar keep-alive al terminar
- **GIVEN** un gateway en ejecución con al menos un backend con `warmup`
- **WHEN** el proceso recibe SIGINT o SIGTERM
- **THEN** el gateway aborta las tareas de keep-alive y finaliza de forma ordenada

#### Scenario: Sin backends con warmup
- **GIVEN** un gateway sin ningún backend con `warmup`
- **WHEN** el proceso recibe SIGINT o SIGTERM
- **THEN** el gateway finaliza de forma ordenada sin tareas de keep-alive que abortar

### Requirement: Failover cuando la petición exige un tool call y la respuesta no lo trae

Cuando la petición incluya `tools` como lista no vacía y `tool_choice` exija una llamada a herramienta (`required`, `any` o un objeto que nombre una función), en modo **no-stream**, el gateway SHALL considerar intento fallido toda respuesta de éxito (`2xx`) cuyo cuerpo no contenga `tool_calls` en `choices[*].message`, y SHALL continuar con el siguiente backend de la cadena. Con `tool_choice` igual a `auto`, `none` o ausente, el gateway SHALL NOT aplicar esta condición. La condición SHALL NOT aplicarse en modo `stream: true`.

Si todos los backends de la cadena se agotan sin una respuesta con `tool_calls`, el gateway SHALL responder un error en formato OpenAI y SHALL NOT devolver la respuesta sin `tool_calls`.

**Scenarios:**

#### Scenario: Primario ignora las tools → failover
- **GIVEN** una ruta con primario y fallback, y una petición con `tools` y `tool_choice: "required"`
- **WHEN** el primario responde `200` sin `tool_calls` y el fallback responde `200` con `tool_calls`
- **THEN** el gateway devuelve la respuesta del fallback
- **AND** `x-gateway-backend` identifica el fallback

#### Scenario: tool_choice auto no dispara failover
- **GIVEN** una petición con `tools` y `tool_choice: "auto"`
- **WHEN** el primario responde `200` sin `tool_calls`
- **THEN** la respuesta del primario se devuelve tal cual
- **AND** NO se contacta el fallback

#### Scenario: tool_choice ausente no dispara failover
- **GIVEN** una petición con `tools` pero sin `tool_choice`
- **WHEN** el primario responde `200` sin `tool_calls`
- **THEN** la respuesta se devuelve y NO se contacta el fallback

#### Scenario: Función concreta exigida
- **GIVEN** una petición con `tool_choice` como objeto que nombra una función
- **WHEN** el primario responde `200` sin `tool_calls`
- **THEN** el gateway hace failover al siguiente backend

#### Scenario: Primario responde con tool_calls → sin failover
- **GIVEN** una petición con `tools` y `tool_choice: "required"`
- **WHEN** el primario responde `200` con `tool_calls`
- **THEN** la respuesta del primario se devuelve tal cual
- **AND** NO se contacta el fallback

#### Scenario: Todos sin tool call
- **GIVEN** todos los backends responden `200` sin `tool_calls`
- **WHEN** se agota la cadena
- **THEN** el gateway responde error estilo OpenAI con código `no_tool_call`
- **AND** NO devuelve ninguna respuesta sin `tool_calls`

#### Scenario: Streaming no aplica la condición
- **GIVEN** `stream: true` con `tools` y `tool_choice: "required"`
- **WHEN** el primario abre el stream
- **THEN** el gateway lo reenvía sin inspeccionar `tool_calls`
- **AND** NO hace failover por esta condición

### Requirement: Configuración de exportación OTLP

El gateway SHALL aceptar un bloque opcional `observability.otlp` en el fichero de configuración. Si el bloque está ausente, el gateway SHALL NOT inicializar ningún exportador y SHALL mantener el comportamiento actual. La validación SHALL rechazar: `endpoint` vacío o en blanco, `organization` vacío o en blanco, `interval_ms` o `timeout_ms` presentes e iguales a 0, `signals` con todas las señales a `false`, y un `token_env` definido cuya variable de entorno no exista.

**Scenarios:**

#### Scenario: Ausencia del bloque no cambia nada
- **GIVEN** un fichero de configuración sin el bloque `observability`
- **WHEN** el gateway arranca
- **THEN** no se inicializa ningún exportador OTLP
- **AND** `GET /metrics` sigue operativo con el contrato actual

#### Scenario: Endpoint vacío se rechaza
- **GIVEN** un bloque `observability.otlp` con `endpoint: ""`
- **WHEN** se carga la configuración
- **THEN** la carga falla con un error de configuración

#### Scenario: Intervalo cero se rechaza
- **GIVEN** un bloque `observability.otlp` con `interval_ms: 0`
- **WHEN** se carga la configuración
- **THEN** la carga falla con un error de configuración

#### Scenario: Token de ingesta inexistente se rechaza
- **GIVEN** un bloque `observability.otlp` con `token_env: OPENOBSERVE_TOKEN` y sin esa variable en el entorno
- **WHEN** el gateway arranca
- **THEN** el arranque falla con un error de configuración

### Requirement: Exportación de métricas por OTLP

Cuando `signals.metrics` sea `true`, el gateway SHALL exportar sus métricas por OTLP a `{endpoint}/api/{organization}/v1/metrics` cada `interval_ms` (por defecto 15000 ms). La exportación SHALL conservar los nombres y la semántica de las series actuales (`llm_gateway_*`): counters acumulativos, un gauge y los histogramas de latencia. Cuando `signals.metrics` sea `false`, el gateway SHALL NOT exportar métricas.

**Scenarios:**

#### Scenario: Métricas llegan al receptor OTLP
- **GIVEN** `signals.metrics = true` y un receptor OTLP de prueba
- **WHEN** se registran peticiones y transcurre un intervalo de exportación
- **THEN** el receptor recibe data points cuyos nombres y valores coinciden con las series registradas

#### Scenario: Métricas desactivadas no exportan
- **GIVEN** `signals.metrics = false`
- **WHEN** se registran peticiones y transcurre un intervalo de exportación
- **THEN** el receptor no recibe ninguna exportación de métricas

### Requirement: Exportación de logs por OTLP

Cuando `signals.logs` sea `true`, el gateway SHALL exportar sus eventos `tracing` por OTLP a `{endpoint}/api/{organization}/v1/logs`, conservando los campos estructurados actuales (`request_id`, `route`, `streaming`, `outcome`, `attempts`, `latency_ms`, etc.). Cuando `signals.logs` sea `false`, el gateway SHALL mantener únicamente la salida local de logs.

**Scenarios:**

#### Scenario: Un evento de petición se exporta como log OTLP
- **GIVEN** `signals.logs = true` y un receptor OTLP de prueba
- **WHEN** se atiende una petición
- **THEN** el receptor recibe un log con los campos `request_id`, `route` y `latency_ms`

### Requirement: Exportación de trazas por OTLP y propagación de contexto

Cuando `signals.traces` sea `true`, el gateway SHALL crear un span por petición entrante y spans de cliente hacia los backends, y SHALL exportarlos por OTLP a `{endpoint}/api/{organization}/v1/traces`. El gateway SHALL propagar el contexto de traza mediante el estándar W3C Trace Context: SHALL extraer `traceparent` de la petición entrante y SHALL inyectarlo en las peticiones a los backends.

**Scenarios:**

#### Scenario: traceparent se propaga al backend
- **GIVEN** `signals.traces = true` y una petición con cabecera `traceparent`
- **WHEN** el gateway reenvía al backend
- **THEN** la petición al backend incluye un `traceparent` con el mismo `trace-id`

#### Scenario: Spans de servidor y cliente se exportan
- **GIVEN** `signals.traces = true`
- **WHEN** se atiende una petición con failover
- **THEN** el receptor recibe un span de servidor y un span por intento contra backend

### Requirement: Credenciales de ingesta por entorno

El gateway SHALL enviar la cabecera `Authorization: Basic <token>` en las exportaciones OTLP cuando `token_env` esté definido, con el valor leído de esa variable de entorno. El gateway SHALL NOT admitir credenciales embebidas en el fichero de configuración.

**Scenarios:**

#### Scenario: Cabecera Authorization presente
- **GIVEN** `token_env` definido y la variable con valor `abc123`
- **WHEN** el gateway exporta a OpenObserve
- **THEN** la petición incluye `Authorization: Basic abc123`

### Requirement: Apagado ordenado con flush

Ante SIGINT o SIGTERM, el gateway SHALL forzar el flush y el shutdown de los providers OTLP antes de terminar, de forma que las señales pendientes no se pierdan.

**Scenarios:**

#### Scenario: Flush en el cierre
- **GIVEN** telemetría OTLP activa con datos pendientes
- **WHEN** el gateway recibe SIGTERM
- **THEN** realiza el flush de los providers antes de finalizar el proceso

### Requirement: Descubrimiento del catálogo free de OpenRouter

El gateway SHALL obtener el catálogo de modelos desde `GET {base_url}/models` sin autenticación, SHALL filtrar a modelos gratuitos (identificador con sufijo `:free` o con `pricing.prompt` y `pricing.completion` iguales a `0`) que además expongan tool-calling (`supported_parameters` contiene `tools` y `tool_choice`), y SHALL almacenarlo en una caché con TTL por `base_url`. El TTL de cada `base_url` SHALL ser el `refresh_interval_ms` común de todos los backends dinámicos que la comparten; si dos backends dinámicos declaran TTLs distintos para la misma `base_url`, la validación de configuración SHALL fallar con un error que nombre la `base_url` y las rutas implicadas. Si un backend dinámico no declara `discovery`, SHALL usar el TTL por defecto. Un catálogo vacío `{"data": []}` SHALL ser válido y producir cero modelos elegibles, sin error. El refresco SHALL ejecutarse en segundo plano con `stale-while-error`.

**Scenarios:**

#### Scenario: Descubrimiento inicial
- **GIVEN** un backend `openrouter_free` sin caché previa
- **WHEN** llega la primera petición
- **THEN** el gateway consulta `/models`, puebla la caché y resuelve la ruta con los modelos elegibles

#### Scenario: Exclusión de modelos de pago
- **GIVEN** un catálogo que contiene modelos con pricing no nulo
- **WHEN** se filtra
- **THEN** esos modelos no aparecen como elegibles

#### Scenario: Exclusión de modelos sin tool-calling
- **GIVEN** un modelo gratuito cuyo `supported_parameters` no contiene `tools` ni `tool_choice`
- **WHEN** se filtra
- **THEN** ese modelo no aparece como elegible

#### Scenario: Fallo de red con caché válida
- **GIVEN** una caché poblada y un fallo al refrescar
- **WHEN** se intenta el refresco
- **THEN** se conserva la última caché válida y las peticiones siguen resolviéndose

#### Scenario: TTL vencido en segundo plano
- **GIVEN** una caché cuyo TTL ha vencido
- **WHEN** llega una petición
- **THEN** el gateway responde con la caché existente y dispara el refresco en segundo plano sin bloquear la petición

#### Scenario: Catálogo vacío
- **GIVEN** `GET /models` devuelve `{"data": []}`
- **WHEN** se consulta el catálogo
- **THEN** la caché queda vacía, no se produce error y la ruta dinámica no aporta candidatos

#### Scenario: TTL vencido combinado con fallo de red
- **GIVEN** una caché con TTL vencido y una petición de refresco que falla
- **WHEN** se solicita la expansión de la ruta dinámica
- **THEN** se usan los modelos de la caché previa si existe, o cero candidatos si nunca hubo catálogo, sin pánico ni error propagado

#### Scenario: TTL incoherente por `base_url`
- **GIVEN** dos backends dinámicos con la misma `base_url` y `refresh_interval_ms` distintos
- **WHEN** se valida la config
- **THEN** error de configuración que indica la `base_url` y las rutas afectadas

#### Scenario: TTL coherente por `base_url`
- **GIVEN** dos backends dinámicos con la misma `base_url` y el mismo `refresh_interval_ms`
- **WHEN** se valida la config
- **THEN** la config se acepta y ambos comparten un único catálogo con ese TTL

### Requirement: Clasificación de modelos por perfil

El gateway SHALL clasificar cada modelo free elegible en los perfiles `architect`, `code` y `writer` mediante una heurística sobre `id`, `name`, `description` y `context_length`, SHALL ordenar cada perfil por tier, después por `context_length` descendente y, a igualdad, por `id` ascendente, y SHALL permitir reclasificaciones manuales por configuración (`exclude`/`include`). Un `include` que nombra un modelo no elegible (ausente del catálogo, filtrado por `min_context` o por `exclude`) SHALL ignorarse en la lista resultante y SHALL emitir un `tracing::warn!` que incluya el identificador del modelo, el perfil y la `base_url`. La clasificación no SHALL cambiar su salida por efecto del aviso.

**Scenarios:**

#### Scenario: `architect` prioriza razonamiento
- **GIVEN** un catálogo con un modelo de la familia de razonamiento (p. ej. `deepseek-r1`, `qwq`, `o3`, `reasoner`) y varios generales
- **WHEN** se clasifica el perfil `architect`
- **THEN** el modelo de razonamiento queda en el primer lugar

#### Scenario: `architect` cae a general grande
- **GIVEN** un catálogo sin modelos de razonamiento elegibles pero con un general grande (p. ej. `llama-3.3-70b`, `qwen-*72b`, `deepseek-v3`)
- **WHEN** se clasifica el perfil `architect`
- **THEN** el general grande queda como primer candidato

#### Scenario: `code` prioriza familias coder
- **GIVEN** un catálogo con un modelo de familia coder (p. ej. `coder`, `codestral`, `devstral`) y varios generales
- **WHEN** se clasifica el perfil `code`
- **THEN** el modelo coder queda en el primer lugar

#### Scenario: `writer` prioriza instruct general
- **GIVEN** un catálogo con modelos coder y generales
- **WHEN** se clasifica el perfil `writer`
- **THEN** los modelos generales instruct preceden a los exclusivamente coder

#### Scenario: Orden determinista
- **GIVEN** dos modelos elegibles con distinto `context_length`
- **WHEN** se clasifica un perfil
- **THEN** el de mayor `context_length` precede al de menor

#### Scenario: `min_context` descarta modelos pequeños
- **GIVEN** `discovery.min_context` configurado y un modelo elegible con `context_length` inferior
- **WHEN** se clasifica
- **THEN** ese modelo no aparece en ningún perfil

#### Scenario: Override manual
- **GIVEN** un override de configuración que excluye un modelo del perfil `code`
- **WHEN** se clasifica el perfil `code`
- **THEN** ese modelo no aparece en la lista resultante

#### Scenario: `include` antepone un modelo elegible
- **GIVEN** un `include` con un modelo presente y elegible
- **WHEN** se clasifica
- **THEN** ese modelo aparece el primero y no se duplica

#### Scenario: `include` no elegible avisa y se descarta
- **GIVEN** un `include` con un modelo ausente del catálogo o filtrado por `min_context`
- **WHEN** se clasifica
- **THEN** el modelo no aparece en la lista y se emite un warning con su identificador, el perfil y la `base_url`

### Requirement: Expansión y rotación de backends dinámicos

En cada petición, el gateway SHALL expandir cada `Backend` de tipo `openrouter_free` a la lista de modelos concretos clasificados para su perfil, en la misma posición que ocupa dentro de la ruta, SHALL excluir los modelos en blacklist y SHALL aplicar rotación round-robin entre peticiones sucesivas, de modo que el orden de expansión avance en cada petición. El candidato resultante SHALL ser una estructura ligera que contenga únicamente el índice de slot, `model`, `base_url`, `timeout_ms`, `api_key_env` y el cooldown de blacklist aplicable; la expansión no SHALL clonar el `Backend` completo (ni `discovery`, ni `overrides`, ni `warmup`). La resolución del modelo, la cabecera de autenticación, el timeout y el marcado en blacklist SHALL producir exactamente los mismos valores que antes del cambio.

**Scenarios:**

#### Scenario: Expansión en posición
- **GIVEN** una ruta con un backend dinámico entre dos backends estáticos
- **WHEN** se resuelve la ruta
- **THEN** los modelos expandidos aparecen entre el primero y el último backend estático

#### Scenario: Rotación round-robin
- **GIVEN** una ruta con un backend dinámico y varios modelos elegibles
- **WHEN** llegan peticiones sucesivas
- **THEN** el primer modelo de la lista rota de forma round-robin entre peticiones

#### Scenario: Exclusión de blacklist
- **GIVEN** un modelo marcado en blacklist
- **WHEN** se expande el backend dinámico
- **THEN** ese modelo no aparece entre los candidatos

#### Scenario: Perfil sin modelos elegibles
- **GIVEN** un perfil sin ningún modelo elegible tras el filtrado
- **WHEN** se resuelve la ruta
- **THEN** el backend dinámico no aporta candidatos y la ruta continúa con el siguiente backend

#### Scenario: Candidato ligero sin clon del backend
- **GIVEN** una ruta con un backend dinámico con `discovery.overrides` y `warmup` declarados
- **WHEN** se expande la ruta
- **THEN** cada candidato expone solo `model`, `base_url`, `timeout_ms` y `api_key_env` (más slot y cooldown) y la petición se envía con el `model` expandido y la `api_key_env` del backend original

#### Scenario: Blacklist compartida entre rutas
- **GIVEN** dos rutas con la misma `base_url` y perfiles distintos
- **WHEN** un modelo se marca en blacklist desde la primera ruta
- **THEN** la segunda ruta tampoco lo propone mientras dure el cooldown

#### Scenario: `/v1/models` deduplica sin duplicados
- **GIVEN** dos backends dinámicos con perfiles distintos que producen modelos solapados
- **WHEN** se consulta `GET /v1/models`
- **THEN** cada identificador aparece una sola vez

### Requirement: Blacklist dinámica de modelos

El gateway SHALL marcar un modelo como no elegible durante `blacklist_cooldown_ms` cuando una petición dirigida a ese modelo devuelva `429`, un código `5xx` o un encabezado `Retry-After`, y SHALL compartir esa blacklist entre todas las rutas.

**Scenarios:**

#### Scenario: 429 marca el modelo
- **GIVEN** una petición a un modelo que responde `429`
- **WHEN** se procesa la respuesta
- **THEN** el modelo entra en blacklist durante `blacklist_cooldown_ms`

#### Scenario: 5xx marca el modelo
- **GIVEN** una petición a un modelo que responde `503`
- **WHEN** se procesa la respuesta
- **THEN** el modelo entra en blacklist

#### Scenario: Retry-After respetado
- **GIVEN** una respuesta con encabezado `Retry-After`
- **WHEN** se calcula el cooldown
- **THEN** el modelo queda en blacklist al menos durante el tiempo indicado

#### Scenario: Expiración del cooldown
- **GIVEN** un modelo en blacklist cuyo cooldown ha vencido
- **WHEN** se expande un backend dinámico
- **THEN** ese modelo vuelve a ser elegible

#### Scenario: Blacklist compartida
- **GIVEN** un modelo marcado en blacklist por una ruta
- **WHEN** otra ruta intenta usar el mismo modelo
- **THEN** el modelo sigue excluido

### Requirement: Fallback y agotamiento de ruta

El gateway SHALL continuar al siguiente backend de la ruta cuando un backend dinámico no aporte candidatos o todos sus modelos fallen, SHALL usar los backends de pago declarados como respaldo y, si se agota la ruta completa, SHALL responder con un error `AllBackendsFailed`.

**Scenarios:**

#### Scenario: Free agotados, fallback de pago
- **GIVEN** una ruta con un backend dinámico y un backend de pago posterior, y todos los modelos free en blacklist o fallando
- **WHEN** se procesa la petición
- **THEN** la petición se sirve con el backend de pago

#### Scenario: Ruta sin backends utilizables
- **GIVEN** una ruta cuyos backends dinámicos no aportan candidatos y sin backends estáticos disponibles
- **WHEN** se procesa la petición
- **THEN** el gateway responde `AllBackendsFailed`

#### Scenario: Failover antes del primer token
- **GIVEN** un modelo free que falla antes de emitir el primer token
- **WHEN** se procesa la petición
- **THEN** el gateway prueba el siguiente candidato de la ruta sin haber enviado nada al cliente

### Requirement: Métricas de descubrimiento

El gateway SHALL exponer métricas con el prefijo `llm_gateway_discovery_` que incluyan, al menos, el número de modelos del catálogo, el número de modelos free elegibles, el número de elegibles por perfil, el resultado de los refrescos y el tamaño de la blacklist, integradas en la exportación OTLP/Prometheus existente. El gauge `llm_gateway_discovery_blacklist` SHALL reflejar el número de modelos en blacklist tras purgar los cooldowns vencidos en cada acceso, y su escritura SHALL omitirse cuando el tamaño no ha cambiado desde la última escritura, sin alterar el valor observable.

**Scenarios:**

#### Scenario: Métricas tras descubrimiento
- **GIVEN** un descubrimiento completado
- **WHEN** se consultan las métricas
- **THEN** aparecen el total de modelos y los elegibles por perfil

#### Scenario: Fallo de refresco
- **GIVEN** un refresco fallido
- **WHEN** se consultan las métricas
- **THEN** el contador de fallos de refresco aumenta

#### Scenario: Tamaño de blacklist
- **GIVEN** uno o más modelos en blacklist
- **WHEN** se consultan las métricas
- **THEN** el tamaño de la blacklist refleja el número de modelos excluidos

#### Scenario: Gauge refleja una marca
- **GIVEN** una blacklist vacía
- **WHEN** se marca un modelo
- **THEN** el gauge vale 1

#### Scenario: Gauge vuelve a cero al expirar
- **GIVEN** un modelo marcado con cooldown corto
- **WHEN** vence el cooldown y se consulta la blacklist
- **THEN** el gauge vale 0

#### Scenario: Escritura solo ante cambios
- **GIVEN** una blacklist con un modelo marcado
- **WHEN** se realizan accesos sucesivos sin cambios de tamaño
- **THEN** el gauge conserva el mismo valor y no se reescribe en cada acceso

### Requirement: Gate de cuota diaria free de OpenRouter

El gateway SHALL dejar de proponer modelos de un backend dinámico `openrouter_free` mientras la cuota diaria de modelos free de la cuenta esté agotada. El gateway SHALL abrir un gate global *free tier agotado hasta T* cuando (a) una respuesta `429` de OpenRouter indique agotamiento de la cuota **diaria** (mediante `error.metadata.limit_source` igual a `openrouter_free_tier_daily`, o mediante `X-RateLimit-Remaining` igual a `0` junto con un `X-RateLimit-Reset` a más de cinco minutos vista), o (b) el sondeo de cuota observe `free_model_daily_requests.remaining` igual a `0`. El gate SHALL ser global (compartido entre rutas y `base_url`) y SHALL NOT afectar a los backends `static`.

El deadline T SHALL ser el `X-RateLimit-Reset` indicado (epoch en milisegundos) o, en su defecto, el siguiente `00:00 UTC`, acotado a un máximo de 24 h. Mientras el gate esté abierto, un backend `openrouter_free` SHALL expandirse a **cero** candidatos y la ruta SHALL continuar con el siguiente backend (típicamente el de pago). Al vencer T, los modelos free SHALL volver a ser elegibles sin intervención.

Un `429` que **no** indique cuota diaria agotada (p. ej. `openrouter_free_tier_per_minute` o `upstream_provider_shared_pool`) SHALL NOT abrir el gate y SHALL seguir el tratamiento de blacklist por modelo existente.

**Scenarios:**

#### Scenario: 429 de cuota diaria abre el gate
- **GIVEN** un backend `openrouter_free` cuya petición devuelve `429` con `limit_source: openrouter_free_tier_daily` y `X-RateLimit-Reset` a 6 h
- **WHEN** se procesa la respuesta
- **THEN** el gate queda abierto hasta ese reset
- **AND** las peticiones siguientes a cualquier ruta dinámica no proponen modelos free y usan el backend de pago

#### Scenario: Sondeo proactivo abre el gate sin 429
- **GIVEN** el sondeo de cuota habilitado y `GET /api/v1/key` con `free_model_daily_requests.remaining = 0`
- **WHEN** se ejecuta el sondeo
- **THEN** el gate se abre hasta el siguiente `00:00 UTC`
- **AND** no se emite ningún intento free posterior

#### Scenario: Gate abierto no propone candidatos free
- **GIVEN** el gate abierto
- **WHEN** se expande un backend dinámico en cualquier ruta
- **THEN** aporta cero candidatos
- **AND** la ruta continúa con el backend de pago declarado

#### Scenario: 429 de minuto no abre el gate
- **GIVEN** un `429` con `limit_source: openrouter_free_tier_per_minute`
- **WHEN** se procesa
- **THEN** el gate permanece cerrado
- **AND** el modelo se blacklistea según la regla por modelo existente

#### Scenario: 429 de proveedor no abre el gate
- **GIVEN** un `429` con `error.metadata.provider_code` presente (límite del proveedor)
- **WHEN** se procesa
- **THEN** el gate permanece cerrado y el modelo sigue el tratamiento de blacklist por modelo

#### Scenario: Expiración del gate reactiva el free
- **GIVEN** el gate abierto hasta T
- **WHEN** transcurre T
- **THEN** los modelos free vuelven a proponerse con normalidad

#### Scenario: Gate sin cabeceras cae a 00:00 UTC
- **GIVEN** un `429` de cuota diaria sin `X-RateLimit-Reset` pero con `limit_source: openrouter_free_tier_daily`
- **WHEN** se abre el gate
- **THEN** T es el siguiente `00:00 UTC`

#### Scenario: Config desactiva el gate
- **GIVEN** `discovery.respect_daily_quota: false`
- **WHEN** llega un `429` de cuota diaria
- **THEN** el gate no se abre y se aplica solo la blacklist por modelo

### Requirement: Sondeo de la cuota free de OpenRouter

Cuando el sondeo esté habilitado, el gateway SHALL consultar `GET {base_url}/key` con la API key del backend dinámico, cada `quota_poll_interval_ms` (default 60000), y SHALL leer `free_model_daily_requests.remaining`. Una llamada por `base_url` SHALL compartirse entre todos los backends dinámicos que la usan. Un fallo del sondeo SHALL NOT alterar el estado del gate (stale-while-error: se conserva el último estado conocido) y SHALL registrarse como fallo de métrica. El sondeo SHALL NOT consumir cuota de modelos free.

**Scenarios:**

#### Scenario: Sondeo periódico
- **GIVEN** `quota_poll_interval_ms = 50` y `remaining > 0`
- **WHEN** transcurren ~200 ms
- **THEN** el gateway ha consultado `/key` al menos dos veces y el gate sigue cerrado

#### Scenario: Fallo de sondeo conserva el estado
- **GIVEN** el gate cerrado y un sondeo que falla por red
- **WHEN** se procesa el fallo
- **THEN** el gate permanece cerrado y no se emite error al cliente

#### Scenario: Una llamada por base_url
- **GIVEN** tres backends dinámicos con la misma `base_url` y el sondeo habilitado
- **WHEN** se ejecuta un ciclo de sondeo
- **THEN** solo se realiza una consulta a `/key` para esa `base_url`

### Requirement: Métricas de la cuota free

El gateway SHALL exponer un gauge `llm_gateway_free_quota_remaining{base_url}` con el último `remaining` observado, y un counter `llm_gateway_free_quota_skips_total{route}` que cuente las veces que una ruta ha saltado el tier free por el gate abierto. Cuando el sondeo esté deshabilitado y no se haya observado cuota, el gateway SHALL NOT emitir el gauge.

**Scenarios:**

#### Scenario: Gauge refleja la cuota observada
- **GIVEN** un sondeo que devuelve `remaining = 250`
- **WHEN** se consulta `GET /metrics`
- **THEN** `llm_gateway_free_quota_remaining{base_url="https://openrouter.ai/api/v1"}` vale `250`

#### Scenario: Contador de saltos
- **GIVEN** el gate abierto
- **WHEN** una petición de la ruta `code` salta el tier free
- **THEN** `llm_gateway_free_quota_skips_total{route="code"}` incrementa en 1

#### Scenario: Sin sondeo no hay gauge
- **GIVEN** `respect_daily_quota: false` y sin observación de cuota
- **WHEN** se consulta `GET /metrics`
- **THEN** no aparece ninguna serie `llm_gateway_free_quota_remaining`

### Requirement: Backend de pago dinámico `openrouter_paid`

El gateway SHALL soportar un backend dinámico de pago declarado con `kind: openrouter_paid`, que reutiliza el catálogo descubierto por `GET {base_url}/models` (con su TTL y su caché compartida por `base_url`) para proponer modelos de pago.

Para ser elegible, un modelo SHALL: coincidir con al menos un patrón de `discovery.allow`; soportar tool-calling (declarar `tools` y `tool_choice` en `supported_parameters`); tener precio estrictamente positivo (`pricing.prompt > 0` o `pricing.completion > 0`), lo que SHALL excluir el tier free; tener `context_length >= min_context`; y no ser un modelo `:batch`.

El gateway SHALL ordenar los elegibles por precio ascendente usando `blend = 0.75 · pricing.prompt + 0.25 · pricing.completion`, con desempate por `context_length` descendente y `id` ascendente. El primer candidato SHALL ser el modelo elegido y el resto SHALL ser el orden de failover. A diferencia de `openrouter_free`, el backend `openrouter_paid` SHALL NOT aplicar rotación round-robin.

**Scenarios:**

#### Scenario: Orden por precio ascendente
- **GIVEN** dos modelos elegibles con `blend` 0.0484 y 0.0872
- **WHEN** se expande el backend de pago
- **THEN** el primero de la lista es el de `blend` 0.0484
- **AND** el segundo es el de `blend` 0.0872

#### Scenario: Excluye el tier free
- **GIVEN** un modelo que coincide con un patrón de `discovery.allow` pero tiene `pricing.prompt = 0` y `pricing.completion = 0`
- **WHEN** se calculan los elegibles
- **THEN** el modelo queda excluido

#### Scenario: Exige tool-calling y contexto mínimo
- **GIVEN** un modelo con precio positivo que no declara `tools` o cuyo `context_length` es inferior a `min_context`
- **WHEN** se calculan los elegibles
- **THEN** el modelo queda excluido

#### Scenario: Sin coincidencias con la lista curada
- **GIVEN** `discovery.allow` con patrones que no coinciden con ningún modelo del catálogo
- **WHEN** se expande el backend de pago
- **THEN** aporta cero candidatos
- **AND** la ruta continúa con el siguiente backend declarado (el `static` final si existe)

#### Scenario: Sin rotación entre expansiones
- **GIVEN** un backend de pago con varios modelos elegibles
- **WHEN** se expande dos veces seguidas
- **THEN** el primer candidato es el mismo en ambas expansiones

### Requirement: Lista curada de modelos de pago (`discovery.allow`)

El gateway SHALL aceptar en `discovery` una lista `allow` de patrones glob aplicados sobre el `model_id` (`*` coincide con cero o más caracteres). Un backend `openrouter_paid` SHALL proponer únicamente modelos que coincidan con al menos un patrón. Un patrón vacío SHALL ser error de configuración. Un patrón válido sin coincidencias en el catálogo SHALL NOT ser error y SHALL registrarse como aviso. Si `allow` está ausente o vacío, el backend SHALL aportar cero candidatos. La lista SHALL poder modificarse sin recompilar el binario ni tocar la config de runtime.

**Scenarios:**

#### Scenario: El comodín absorbe versiones menores
- **GIVEN** `allow: ["z-ai/glm-5.*-flash"]`
- **WHEN** el catálogo contiene `z-ai/glm-5.3-flash`
- **THEN** ese modelo es candidato

#### Scenario: Patrón vacío es error
- **GIVEN** `allow: [""]`
- **WHEN** se carga la configuración
- **THEN** la carga falla con un error que identifica el patrón vacío

#### Scenario: Patrón sin coincidencias avisa pero no falla
- **GIVEN** `allow: ["lab-inexistente/modelo-x"]`
- **WHEN** se expande el backend de pago con el catálogo cargado
- **THEN** no hay error de configuración
- **AND** se registra un aviso con el patrón sin coincidencias

### Requirement: Selección pegajosa del modelo de pago

El gateway SHALL mantener el modelo de pago elegido en cada `(route, backend)` y SHALL cambiarlo solo cuando concurra alguna de estas condiciones: (a) el modelo pegajoso deja de ser elegible o queda blacklisted, en cuyo caso SHALL pasar de inmediato al más barato disponible; o (b) ha transcurrido al menos `reselect_interval_ms` (default 14400000, 4 h) desde la elección **y** el modelo más barato disponible es al menos `min_price_delta` (default 0.20) más barato en términos relativos. El refresco del catálogo (`refresh_interval_ms`) SHALL NOT, por sí solo, cambiar el modelo elegido. El estado de selección SHALL NOT persistir entre reinicios: la primera elección tras arrancar SHALL ser el modelo más barato elegible.

**Scenarios:**

#### Scenario: Mantiene el modelo dentro de reselect_interval_ms
- **GIVEN** el modelo pegajoso `A` elegido hace 10 min y un candidato `B` más barato
- **WHEN** se expande el backend de pago
- **THEN** el primer candidato sigue siendo `A`

#### Scenario: Cambia tras reselect_interval_ms si hay mejora suficiente
- **GIVEN** el modelo pegajoso `A` elegido hace más de `reselect_interval_ms` y un candidato `B` un 30 % más barato (`min_price_delta` 0.20)
- **WHEN** se expande el backend de pago
- **THEN** el primer candidato pasa a ser `B`

#### Scenario: No cambia si la mejora es marginal
- **GIVEN** el modelo pegajoso `A` elegido hace más de `reselect_interval_ms` y un candidato `B` solo un 5 % más barato
- **WHEN** se expande el backend de pago
- **THEN** el primer candidato sigue siendo `A`

#### Scenario: Cambia de inmediato si el modelo falla o desaparece
- **GIVEN** el modelo pegajoso `A` y `A` pasa a no ser elegible (blacklisted o ausente del catálogo)
- **WHEN** se expande el backend de pago
- **THEN** el primer candidato es el más barato disponible

#### Scenario: Tras reinicio elige el más barato
- **GIVEN** un gateway recién arrancado sin estado de selección
- **WHEN** se expande el backend de pago por primera vez
- **THEN** el primer candidato es el más barato elegible de la lista curada

### Requirement: Ejemplo con respaldo de pago dinámico y red de seguridad estática

El fichero `gateway.yaml.example` SHALL declarar, en las rutas dinámicas (`architect`, `code`, `writer`), un backend `kind: openrouter_paid` con el `profile` correspondiente, y SHALL incluir una `discovery.allow` por defecto formada por patrones de modelos *flash* de frontera. El ejemplo SHALL conservar además un backend `static` final como último recurso. La documentación de rutas free SHALL reflejar este diseño y la lista de patrones.

**Scenarios:**

#### Scenario: El ejemplo usa el backend dinámico con static final
- **GIVEN** el fichero `gateway.yaml.example`
- **WHEN** se inspeccionan los backends de la ruta `code`
- **THEN** hay un backend `kind: openrouter_paid` con `profile: code`
- **AND** tras él hay un backend `kind: static` como último recurso
- **AND** `discovery.allow` contiene el patrón `z-ai/glm-5.*-flash`
