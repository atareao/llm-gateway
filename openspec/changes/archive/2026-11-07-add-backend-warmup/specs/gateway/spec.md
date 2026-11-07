## ADDED Requirements

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
