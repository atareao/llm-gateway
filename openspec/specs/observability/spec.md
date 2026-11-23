# observability Specification

## Purpose
La capability `observability` convierte el uso del gateway en un ledger consultable en OpenObserve: registra la evolución de precios del catálogo de OpenRouter (`gateway_catalog`, solo al cambiar), qué modelo se eligió en cada ruta y por qué cambió (`gateway_selection`), y el resultado de cada petición —tokens, coste estimado, latencia y `finish_reason`— (`gateway_requests`). Nace de que el catálogo cambia a diario (315 altas en 2026) y el respaldo de pago dinámico puede cambiar de modelo sin dejar rastro, y de que el gateway no parseaba `usage` en ningún punto, por lo que no había coste real medible. Los tres eventos se emiten como log estructurado (OTLP), son deduplicados en memoria para no inundar el sink y se pueden desactivar por completo con `observability.events: false` sin afectar a las métricas Prometheus.

## Requirements

### Requirement: Parseo de `usage` en las respuestas

El gateway SHALL extraer `prompt_tokens` y `completion_tokens` del objeto `usage` de la respuesta de OpenRouter, tanto en modo no-stream (leyendo el body de la respuesta) como en streaming (inyectando `stream_options.include_usage: true` en la petición y leyendo el `usage` del chunk final). Cuando la respuesta no incluya `usage`, el gateway SHALL marcar los tokens como **desconocidos** y SHALL NOT inventar el valor `0`.

**Scenarios:**

#### Scenario: Respuesta no-stream con usage
- **GIVEN** una petición no-stream cuya respuesta incluye `usage.prompt_tokens` y `usage.completion_tokens`
- **WHEN** el gateway procesa la respuesta
- **THEN** los tokens de entrada y de salida quedan registrados con esos valores

#### Scenario: Streaming inyecta include_usage
- **GIVEN** una petición con `stream: true`
- **WHEN** el gateway reenvía la petición
- **THEN** el cuerpo enviado al upstream incluye `stream_options.include_usage: true`
- **AND** el gateway lee el `usage` del chunk final y registra los tokens

#### Scenario: Sin usage los tokens quedan desconocidos
- **GIVEN** una respuesta sin objeto `usage`
- **WHEN** el gateway procesa la respuesta
- **THEN** los tokens quedan marcados como desconocidos
- **AND** no se registra un valor `0` como si fuera medido

### Requirement: Evento de catálogo `gateway_catalog`

El gateway SHALL emitir un evento estructurado con `event_type` igual a `catalog` por cada modelo cuyo precio cambie o que aparezca o desaparezca del catálogo, y SHALL NOT emitirlo cuando el precio no cambia (deduplicación en memoria contra el último snapshot observado). El evento SHALL incluir `ts`, `model_id`, `org`, `pin`, `pout`, `blend`, `context_length`, `change_kind` (uno de `new`, `price_up`, `price_down`, `removed`) y, cuando aplique, `prev_pin` y `prev_pout`. `blend` SHALL calcularse como `0.75 · pin + 0.25 · pout` en USD por millón de tokens.

**Scenarios:**

#### Scenario: Cambio de precio emite un evento
- **GIVEN** un modelo cuyo `pout` pasa de 0.1992 a 0.1800
- **WHEN** se procesa el refresco del catálogo
- **THEN** se emite un evento `catalog` con `change_kind: price_down`, `pin`/`pout` nuevos y `prev_pout: 0.1992`

#### Scenario: Precio sin cambios no emite
- **GIVEN** un modelo cuyo precio no ha cambiado respecto al último snapshot
- **WHEN** se procesa el refresco del catálogo
- **THEN** no se emite ningún evento para ese modelo

#### Scenario: Alta y baja de modelo
- **GIVEN** un modelo que aparece en el catálogo y, más tarde, otro que desaparece
- **WHEN** se procesan los refrescos
- **THEN** se emiten sendos eventos con `change_kind: new` y `change_kind: removed`

### Requirement: Evento de selección `gateway_selection`

El gateway SHALL emitir un evento estructurado con `event_type` igual a `selection` cada vez que cambie el modelo elegido de un `(route, backend)`. El evento SHALL incluir `ts`, `route`, `backend`, `model_id`, `reason` (uno de `initial`, `cheaper`, `failover`, `removed`), `pin`, `pout` y, cuando exista, `prev_model_id`. Un cambio con el mismo modelo SHALL NOT emitir evento.

**Scenarios:**

#### Scenario: Elección inicial
- **GIVEN** un gateway que expande por primera vez un backend `openrouter_paid`
- **WHEN** se selecciona el modelo más barato de la lista curada
- **THEN** se emite un evento `selection` con `reason: initial` y sin `prev_model_id`

#### Scenario: Cambio por modelo más barato
- **GIVEN** el modelo pegajoso `A` y, tras `reselect_interval_ms`, un candidato `B` claramente más barato
- **WHEN** se cambia la selección a `B`
- **THEN** se emite un evento `selection` con `reason: cheaper` y `prev_model_id: A`

#### Scenario: Cambio por failover
- **GIVEN** el modelo pegajoso `A` que pasa a estar blacklisted
- **WHEN** se cambia al más barato disponible `B`
- **THEN** se emite un evento `selection` con `reason: failover` y `prev_model_id: A`

### Requirement: Evento de petición `gateway_requests`

El gateway SHALL emitir un evento estructurado con `event_type` igual a `request` por cada petición atendida, con los campos `ts`, `request_id`, `route`, `model_id`, `streaming`, `attempts`, `failover`, `outcome`, `finish_reason`, `latency_ms`, `tokens_in`, `tokens_out`, `pin`, `pout` y `cost_usd`. `cost_usd` SHALL calcularse como `tokens_in · pin + tokens_out · pout` (precios en USD por token) usando los precios del modelo en el momento de la petición; cuando `usage` falte, `tokens_in`, `tokens_out` y `cost_usd` SHALL quedar desconocidos. El `request_id` SHALL NOT usarse como etiqueta de métrica.

**Scenarios:**

#### Scenario: Petición correcta emite evento con coste
- **GIVEN** una petición atendida por el modelo `M` con `tokens_in = 1000`, `tokens_out = 200` y precios conocidos
- **WHEN** se completa la petición
- **THEN** se emite un evento `request` con esos tokens y `cost_usd` igual a la combinación de ambos precios
- **AND** `outcome` refleja el éxito y `finish_reason` el del upstream

#### Scenario: Sin usage el coste queda desconocido
- **GIVEN** una petición cuya respuesta no trae `usage`
- **WHEN** se completa la petición
- **THEN** el evento se emite con `tokens_in`, `tokens_out` y `cost_usd` desconocidos
- **AND** el resto de campos (route, model_id, latencia, outcome) se registran igualmente

#### Scenario: request_id no es etiqueta de métrica
- **GIVEN** una petición atendida
- **WHEN** se consulta `GET /metrics`
- **THEN** ninguna serie incluye `request_id` como etiqueta

### Requirement: Métricas de tokens y coste

El gateway SHALL exponer las métricas `llm_gateway_tokens_total{route,model,type}` (con `type` en `prompt` o `completion`), `llm_gateway_cost_usd_total{route,model}`, `llm_gateway_selected_model_price_usd{route,model,side}`, `llm_gateway_allowlist_price_usd{model}` y `llm_gateway_model_selection_total{route,reason}`. El etiquetado SHALL limitarse a `route` y `model` (y `type`/`side`/`reason`); el `request_id` SHALL NOT usarse como etiqueta.

**Scenarios:**

#### Scenario: Tokens y coste incrementan
- **GIVEN** una petición atendida con `tokens_in = 1000` y `tokens_out = 200`
- **WHEN** se consulta `GET /metrics`
- **THEN** `llm_gateway_tokens_total{type="prompt"}` incrementa en 1000 y `{type="completion"}` en 200
- **AND** `llm_gateway_cost_usd_total` incrementa en el coste calculado

#### Scenario: Gauge del modelo seleccionado
- **GIVEN** la ruta `code` con el modelo `M` seleccionado a un precio conocido
- **WHEN** se consulta `GET /metrics`
- **THEN** `llm_gateway_selected_model_price_usd{route="code",model="M",side="prompt"}` refleja el precio de entrada de `M`

#### Scenario: Gauge de la lista curada
- **GIVEN** una lista curada con N modelos con precio conocido
- **WHEN** se consulta `GET /metrics`
- **THEN** existen N series de `llm_gateway_allowlist_price_usd` (una por modelo)

#### Scenario: Contador de selecciones por motivo
- **GIVEN** una transición de selección con `reason: failover`
- **WHEN** se consulta `GET /metrics`
- **THEN** `llm_gateway_model_selection_total{route="code",reason="failover"}` incrementa en 1

### Requirement: Emisión opt-out y retención documentada

El gateway SHALL permitir desactivar la emisión de los tres eventos mediante `observability.events: false` (default `true`). La documentación SHALL especificar la retención recomendada por evento (`catalog` y `selection` entre 1 y 2 años; `requests` entre 30 y 90 días) y SHALL indicar que la retención se configura en OpenObserve, no en el gateway.

**Scenarios:**

#### Scenario: Emisión desactivada
- **GIVEN** `observability.events: false`
- **WHEN** se atiende una petición
- **THEN** no se emite ningún evento `catalog`, `selection` ni `request`

#### Scenario: Retención documentada
- **GIVEN** la documentación del change
- **WHEN** se consulta la sección de retención
- **THEN** describe la retención recomendada por evento y que se configura en OpenObserve

### Requirement: Emisión sin penalizar el hot path

La emisión de los eventos SHALL ser asíncrona (no bloqueante) y SHALL NOT alterar la respuesta enviada al cliente ni el `finish_reason`. Un fallo al emitir un evento SHALL NOT provocar un error en la petición.

**Scenarios:**

#### Scenario: Fallo de emisión no afecta a la respuesta
- **GIVEN** el exportador de logs caído
- **WHEN** se atiende una petición
- **THEN** el cliente recibe su respuesta con normalidad
- **AND** no se registra un error de petición por el fallo de emisión
