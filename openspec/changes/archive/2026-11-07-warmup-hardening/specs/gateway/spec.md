# Spec Delta

## MODIFIED Requirements

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

## ADDED Requirements

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
