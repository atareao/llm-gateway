# Spec Delta

## ADDED Requirements

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

### Requirement: Respaldo de pago con modelo de coste reducido en el ejemplo

El fichero `gateway.yaml.example` SHALL declarar `z-ai/glm-5.3-flash` como modelo de respaldo de pago en las rutas dinámicas (`architect`, `code`, `writer`), en lugar de un placeholder o de `deepseek/deepseek-v4.1-flash`. La documentación de rutas free SHALL reflejar ese modelo.

**Scenarios:**

#### Scenario: El ejemplo usa el modelo barato
- **GIVEN** el fichero `gateway.yaml.example`
- **WHEN** se inspeccionan los backends de pago de las rutas dinámicas
- **THEN** el `model` es `z-ai/glm-5.3-flash`
