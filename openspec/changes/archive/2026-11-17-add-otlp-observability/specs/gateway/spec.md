## ADDED Requirements

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

## MODIFIED Requirements

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