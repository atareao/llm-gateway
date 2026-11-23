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