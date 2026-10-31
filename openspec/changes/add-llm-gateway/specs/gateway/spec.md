# Spec Delta

## Purpose

Proxy local compatible con la API de OpenAI que enruta cada petición a una cadena ordenada de backends y hace failover antes del primer token, permitiendo usar modelos locales sin perder robustez.

## ADDED Requirements

### Requirement: Configuración de rutas por TOML

El gateway se configura con un fichero TOML que define una lista de rutas; cada ruta tiene un `name` (nombre lógico que pide el cliente) y una lista **ordenada** `backend` (primario primero, respaldos después).

- `pub struct GatewayConfig { pub route: Vec<Route> }`
- `pub struct Route { pub name: String, pub backend: Vec<Backend> }`
- `pub struct Backend { pub base_url: String, pub model: String, pub api_key_env: Option<String>, pub timeout_ms: Option<u64> }`

**Scenarios:**

#### Scenario: Config válida
- **GIVEN** un `gateway.toml` con una ruta `mech-writer` y dos backends
- **WHEN** se carga la config
- **THEN** la ruta existe con 2 backends en orden

#### Scenario: Ruta sin backends
- **GIVEN** una ruta con `backend = []`
- **WHEN** se valida
- **THEN** error de configuración indicando el nombre de la ruta

#### Scenario: Fichero ausente
- **GIVEN** que `GATEWAY_CONFIG` apunta a un fichero inexistente
- **WHEN** arranca
- **THEN** el proceso aborta con un error claro

### Requirement: Endpoint compatible con OpenAI

El gateway expone `POST /v1/chat/completions` que acepta el mismo cuerpo que la API de OpenAI.

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

El gateway NO interpreta el cuerpo: lo reenvía tal cual **salvo `model`**.

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

Ante fallo de conexión, timeout, `429` o `5xx` **antes de emitir el primer byte**, el gateway reintenta con el siguiente backend de la cadena.

**Scenarios:**

#### Scenario: Primario caído
- **GIVEN** `mech-writer` con el primario local caído y un fallback remoto
- **WHEN** llega la petición
- **THEN** intenta el primario, falla, y responde desde el fallback
- **AND** la respuesta incluye la cabecera `x-gateway-backend` con el fallback usado

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

**Scenarios:**

#### Scenario: Listado de modelos
- **GIVEN** la config cargada
- **WHEN** `GET /v1/models`
- **THEN** devuelve la lista de nombres lógicos de ruta

#### Scenario: Salud
- **WHEN** `GET /healthz`
- **THEN** responde `200` con `ok`

### Requirement: Secretos solo por entorno

**Scenarios:**

#### Scenario: Clave resuelta en tiempo de petición
- **GIVEN** un backend con `api_key_env = "OPENROUTER_API_KEY"`
- **WHEN** se reenvía
- **THEN** se añade `Authorization: Bearer <valor>`
- **AND** la clave NUNCA aparece en los logs

#### Scenario: Variable ausente
- **GIVEN** que `api_key_env` apunta a una variable no definida
- **THEN** ese backend se considera no disponible y se salta al siguiente
