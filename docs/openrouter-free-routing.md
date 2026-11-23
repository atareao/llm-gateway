# Rutas dinámicas OpenRouter free por perfil

## Qué es

El gateway incorpora **rutas dinámicas de OpenRouter**, especializadas por
**perfil de uso** (`architect`, `code`, `writer`). En lugar de fijar un modelo
concreto en la configuración, un backend de tipo `openrouter_free` descubre el
catálogo público de OpenRouter, filtra los modelos gratuitos que admiten
*tool-calling*, los clasifica según el perfil y los usa como cadena de candidatos
con **failover**, **rotación round-robin** y **blacklist dinámica**.

Sobre ese tier free se añaden dos mecanismos:

- **Guarda de cuota diaria** (`openrouter_free`): el gateway detecta el
  agotamiento de la cuota diaria gratuita (reactivamente ante un `429` diario y,
  proactivamente, sondeando `GET /key`), abre un *gate* global y, mientras está
  abierto, el tier free deja de proponer candidatos y **cuenta los saltos**.
- **Respaldo de pago dinámico** (`openrouter_paid`): en lugar de un modelo de
  pago fijo, un backend `openrouter_paid` reutiliza el mismo catálogo y elige el
  modelo **más barato** de una **lista curada** (`discovery.allow`), con
  **selección pegajosa** e **histéresis de precio**: el modelo no cambia a cada
  refresco, solo cuando la mejora supera un umbral y ha pasado una cadencia.

Es opt-in: las configuraciones existentes siguen funcionando sin cambios
(`kind` por defecto es `static`).

## Ficheros implicados

- `src/config.rs` — `BackendKind`, `Profile`, `Discovery` y `ClassifyOverrides`;
  defaults (`DEFAULT_REFRESH_INTERVAL_MS`, `DEFAULT_BLACKLIST_COOLDOWN_MS`,
  `DEFAULT_QUOTA_POLL_INTERVAL_MS`, `DEFAULT_RESELECT_INTERVAL_MS`,
  `DEFAULT_MIN_PRICE_DELTA`) y validación semántica.
- `src/discovery.rs` — consulta `GET {base_url}/models`, filtro free +
  tool-calling, caché con TTL, refresco en segundo plano (`stale-while-error`),
  gate de cuota diaria (apertura/expiración y sondeo de `GET /key`), selección
  pegajosa del modelo de pago e índice `(route, backend)`, y blacklist global.
- `src/classify.rs` — heurística de clasificación por perfil, `min_context`,
  overrides y la clasificación del tier de pago (`classify_paid`, `matches_allow`,
  `blend_price`).
- `src/proxy.rs` — expansión de backends dinámicos en su posición, rotación,
  blacklist al recibir `429`/`5xx`/`Retry-After`, detección del `429` de cuota
  diaria y fallback / `AllBackendsFailed`.
- `src/metrics.rs` — series `llm_gateway_discovery_*`, el gauge
  `llm_gateway_free_quota_remaining` y el counter
  `llm_gateway_free_quota_skips_total`.

## Cómo funciona

### Descubrimiento

Cada `base_url` dinámica consulta `GET {base_url}/models` (endpoint público, sin
autenticación) con un timeout total de `CATALOG_TIMEOUT` (10 s). Del catálogo se
conservan solo los modelos:

- **gratuitos**: identificador con sufijo `:free` o `pricing.prompt` y
  `pricing.completion` iguales a `0`; y
- **con tool-calling**: `supported_parameters` contiene `tools` y `tool_choice`.

El resultado se guarda en una caché con TTL igual a `refresh_interval_ms`. El
refresco es *stale-while-revalidate*: si el TTL ha vencido, la petición se
resuelve con la copia existente y se dispara el refresco en segundo plano; si el
refresco falla (red, status no exitoso, decodificación), se conserva la última
caché válida (*stale-while-error*). Un único `/models` por `base_url` alimenta a
todos los perfiles.

### Clasificación por perfil

`classify.rs` deriva, para cada perfil, una lista **ordenada** de identificadores
a partir de `id`, `name`, `description` y `context_length`:

- **`architect`**: tier 0 familias de razonamiento (`deepseek-r1`, `reasoner`,
  `thinking`, `qwq`, serie `o1`/`o3`…), tier 1 general grande
  (`70b`/`72b`/`235b`/`405b`, `deepseek-v3`, `llama-3.3`), tier 2 el resto.
- **`code`**: primero familias orientadas a código (`coder`, `codestral`,
  `devstral`, `codeqwen`), después el resto.
- **`writer`**: primero modelos generales instruct, después los exclusivamente
  coder.

Dentro de cada tier se ordena por `context_length` **descendente** y, a
igualdad, por `id` **ascendente** (orden determinista). Los modelos con
`context_length` inferior a `discovery.min_context` se descartan, y los
`overrides` permiten excluir (`exclude`) o anteponer (`include`) modelos
concretos. Un `include` que no es elegible (ausente del catálogo, filtrado por
`min_context` o excluido por `exclude`) se descarta y emite un
`tracing::warn!` con el modelo, el perfil y la `base_url`; la lista resultante
no cambia.

### Expansión y rotación

En cada petición, cada backend `openrouter_free` se **expande en su posición**
dentro de la ruta a los modelos concretos clasificados para su perfil. La
expansión aplica:

- exclusión de los modelos en blacklist;
- **rotación round-robin**: entre peticiones sucesivas el primer modelo de la
  lista avanza, de modo que la carga se reparte entre los elegibles.

Si un perfil no aporta candidatos, el backend dinámico no añade ninguno y la
ruta continúa con el siguiente backend.

### Blacklist dinámica

Cuando una petición a un modelo dinámico devuelve `429`, un `5xx` o incluye el
encabezado `Retry-After`, el modelo entra en una **blacklist global** (compartida
entre todas las rutas) durante `blacklist_cooldown_ms`. Si `Retry-After` está
presente, el cooldown efectivo es el mayor entre el configurado y el indicado
(admitiendo segundos o fecha HTTP); al vencer, el modelo vuelve a ser elegible.
El gauge `llm_gateway_discovery_blacklist` solo se reescribe cuando cambia el
tamaño de la blacklist; los accesos sin cambios (la mayoría de purgas) no
tocan la serie.

### Fallback y agotamiento

La expansión reutiliza el motor `route_with_failover` existente: la ruta se
recorre en orden probando cada candidato **antes del primer byte**; si el tier
free se agota (todo en blacklist o fallando), se usa el respaldo declarado a
continuación. Si se agota la ruta completa, el gateway responde con
`AllBackendsFailed`.

### Guarda de cuota diaria (gate)

La cuota diaria gratuita de OpenRouter se agota de golpe para **toda** la clave
(`limit_source: openrouter_free_tier_daily`). Cuando eso ocurre, seguir
proponiendo modelos free solo produce `429`s en cadena, así que el gateway abre
un **gate global** y, mientras está abierto, ningún backend `openrouter_free`
aporta candidatos: la ruta salta directamente al respaldo de pago.

El gate se abre por dos vías:

- **Reactiva:** al recibir un `429` cuyo `error.metadata.limit_source` es
  `openrouter_free_tier_daily`, o cuando las cabeceras `X-RateLimit-Remaining: 0`
  y un `X-RateLimit-Reset` a más de 5 minutos indican agotamiento diario. Un
  `429` de cuota por minuto o de proveedor **no** abre el gate. El `429` solo
  abre el gate si el backend **respeta** la cuota: un `429` diario de un backend
  con `respect_daily_quota: false` no lo abre aunque otro backend de la ruta sí
  la respete.
- **Proactiva (sondeo):** cada `base_url` free lanza una tarea en segundo plano
  que consulta `GET {base_url}/key` cada `quota_poll_interval_ms` y lee
  `data.free_model_daily_requests.remaining`. Con `remaining == 0` abre el gate
  igual que un `429` diario; con `remaining > 0` **lo cierra** de inmediato (por
  ejemplo justo tras el reset a las `00:00 UTC`, para no bloquear el free hasta
  24 h). El sondeo es *stale-while-error*: un fallo de red conserva el estado.

La expiración se calcula como T = `X-RateLimit-Reset` (epoch en ms) con fallback
al siguiente `00:00 UTC`, topado a `MAX_QUOTA_GATE` (24 h). Al vencer el plazo el
gate se cierra solo y el tier free vuelve a proponerse.

**Política de deadline (deliberada):** si el gate ya está abierto, un `429` con
un deadline anterior **no** lo acorta; se conserva el vencimiento más lejano
(«conservar el deadline más lejano»). Un mismo agotamiento puede llegar por
varias rutas y `base_url` con resets ligeramente distintos, y acortar por el
primero en llegar cerraría el gate antes de que la cuota se recupere de verdad.
Es el sondeo quien puede cerrarlo antes si observa `remaining > 0`.

`respect_daily_quota: false` desactiva gate y sondeo para ese backend. Si dos
backends free comparten `base_url` con `quota_poll_interval_ms` distintos, el
sondeo es único (uno por `base_url`) y se toma el **menor** intervalo, emitiendo
un aviso.

### Métricas de cuota

- `llm_gateway_free_quota_remaining{base_url}` — gauge con el `remaining` del
  último sondeo; solo se emite cuando hay alguna observación.
- `llm_gateway_free_quota_skips_total{route}` — counter que incrementa **una vez
  por petición y ruta** que salta el tier free por tener el gate abierto (no una
  vez por cada backend free de la ruta, y solo si el salto es real: catálogo
  disponible y gate abierto).

### Respaldo de pago dinámico (`openrouter_paid`)

El backend `openrouter_paid` reutiliza **el mismo catálogo cacheado por
`base_url`** que el free (un solo `/models` alimenta a ambos) y elige, de entre
los modelos de la **lista curada** `discovery.allow`, el **más barato**. La
elegibilidad de un modelo de pago exige, además de coincidir con `allow`:

- tool-calling (`tools` + `tool_choice`);
- **precio estrictamente positivo** (`pricing.prompt` o `pricing.completion` >
  0), lo que excluye de forma natural el tier free;
- `context_length` ≥ `discovery.min_context`;
- no ser un modelo `:batch`.

El orden es por **`blend = 0.75 · prompt + 0.25 · completion` ascendente**, con
desempate por `context_length` descendente y `id` ascendente.

`allow` es una lista de **patrones glob** sobre el `id` (`*` coincide con cero o
más caracteres; el resto se compara literalmente). Un patrón **sin ninguna
coincidencia** en el catálogo emite un `tracing::warn!` con el patrón y la
`base_url`, pero **no** es un error de arranque (una lista a medio migrar no debe
tumbar el servicio). El aviso se emite **una sola vez por `(base_url, patrón)`** y
fuera del camino por petición: el `Registry` recuerda los patrones ya avisados
para no inundar el log en cada expansión. Sin `allow` (o con `allow` vacío) el
backend aporta **cero** candidatos y la ruta continúa con el siguiente backend
declarado.

**Selección pegajosa con histéresis.** El `Registry` guarda un estado por
`(route, backend)`: el modelo elegido, el instante de la elección y el `blend`
observado. En cada expansión:

- si no hay estado previo, se elige el modelo más barato (primera elección tras
  reinicio);
- si el modelo elegido sigue siendo elegible y el catálogo no trae uno más
  barato, se **mantiene**;
- si hay un candidato más barato, solo se cambia cuando ha pasado
  `reselect_interval_ms` **y** la mejora relativa alcanza `min_price_delta`
  (`(blend_actual − blend_nuevo) / blend_actual ≥ min_price_delta`); una mejora
  marginal no justifica la migración;
- si el modelo elegido entra en **blacklist** o **desaparece** del catálogo, se
  cambia de inmediato al más barato disponible, sin esperar la cadencia.

El modelo elegido va **al frente** de la lista de candidatos; el resto, en orden
de precio, es el *failover*. A diferencia del tier free, el tier de pago **no
rota** (round-robin): dos expansiones consecutivas devuelven el mismo primer
candidato. El respaldo de pago tampoco pertenece al tier free, así que no cuenta
como salto ni reabre el gate.

## Configuración

Campos nuevos del `Backend` y sus valores por defecto reales:

| Campo | Tipo | Por defecto | Descripción |
|---|---|---|---|
| `kind` | `static` \| `openrouter_free` \| `openrouter_paid` | `static` | Backend fijo o dinámico (free o de pago). |
| `profile` | `architect` \| `code` \| `writer` | `code` | Perfil de clasificación del backend dinámico (en `openrouter_paid` se usa para etiquetar, no para ordenar). |
| `discovery` | bloque | ausente | Configuración del descubrimiento (opcional). |
| `discovery.refresh_interval_ms` | `u64` | `300000` (5 min) | TTL de la caché y periodo de refresco. |
| `discovery.blacklist_cooldown_ms` | `u64` | `60000` (1 min) | Cooldown de la blacklist. |
| `discovery.min_context` | `u32` | sin mínimo (`None`) | Contexto mínimo exigido a los modelos. |
| `discovery.overrides.exclude` | lista | vacía | Modelos excluidos del perfil. |
| `discovery.overrides.include` | lista | vacía | Modelos antepuestos en el perfil. |
| `discovery.respect_daily_quota` | `bool` | `true` | Respeta el gate de cuota diaria free (gate + sondeo). |
| `discovery.quota_poll_interval_ms` | `u64` | `60000` (1 min) | Cadencia del sondeo de `GET /key`. |
| `discovery.allow` | lista | vacía | Lista curada de patrones glob (solo `openrouter_paid`). |
| `discovery.reselect_interval_ms` | `u64` | `14400000` (4 h) | Cadencia mínima de reselección del modelo de pago. |
| `discovery.min_price_delta` | `f64` | `0.20` | Mejora relativa mínima exigida para cambiar de modelo de pago. |

Notas:

- Un backend `openrouter_free` o `openrouter_paid` requiere `base_url` y
  `api_key_env`; su `model` se ignora (lo resuelve el descubrimiento). Un
  `static` requiere `base_url` y `model`.
- El bloque `discovery` es opcional. Cada campo usa su propio default si se
  omite (`refresh_interval_ms: 300000`, `blacklist_cooldown_ms: 60000`,
  `min_context: None`, `overrides: None`, `allow: []`, `reselect_interval_ms:
  14400000`, `min_price_delta: 0.20`), así que `discovery: { min_context: 32768
  }` es válido. Un `0` explícito en los TTL sigue siendo error, igual que un
  `quota_poll_interval_ms: 0`, un `min_price_delta` negativo o no finito, o un
  patrón **vacío** en `allow`.
- El catálogo se comparte por `base_url`: todos los backends dinámicos
  (`openrouter_free` y `openrouter_paid`) que usan la misma `base_url` deben
  declarar el **mismo TTL efectivo** (explícito o por default), en la misma ruta
  o en rutas distintas. Un TTL incoherente hace fallar la carga con un error que
  nombra la `base_url` y las rutas implicadas (fail-fast). Detalle en
  [`docs/openrouter-free-followups.md`](openrouter-free-followups.md).
- El timeout de la consulta del catálogo (`CATALOG_TIMEOUT`, 10 s) es una
  constante del código, no configurable.

Ejemplo de las tres rutas (dinámica free + respaldo de pago dinámico +
`static` final). La `base_url` es compartida, así que el TTL debe coincidir en
todas las declaraciones:

```yaml
route:
  - name: architect
    backend:
      - kind: openrouter_free
        profile: architect
        base_url: "https://openrouter.ai/api/v1"
        api_key_env: "OPENROUTER_API_KEY"
        discovery:
          refresh_interval_ms: 300000
          blacklist_cooldown_ms: 60000
          min_context: 32768
      - kind: openrouter_paid                  # respaldo de pago por perfil
        profile: architect
        base_url: "https://openrouter.ai/api/v1"
        api_key_env: "OPENROUTER_API_KEY"
        discovery:
          refresh_interval_ms: 300000          # mismo TTL (base_url compartida)
          min_context: 32768
          allow:
            - "deepseek/deepseek-v4.1-flash*"
            - "z-ai/glm-5.*-flash"
            - "qwen/qwen3.7-flash"
            - "tencent/hy4-flash"
            - "inclusionai/ling-3.1-flash"
            - "nvidia/nemotron-3.5-lightning"
            - "minimax/minimax-m3.*-flash"
          reselect_interval_ms: 14400000
          min_price_delta: 0.20
      - base_url: "https://openrouter.ai/api/v1"   # último recurso estático
        model: "<REEMPLAZA-modelo-final>"
        api_key_env: "OPENROUTER_API_KEY"
  # ... idéntico para `code` y `writer` cambiando `profile`.
```

La lista curada por defecto, en su orden, es:

```
deepseek/deepseek-v4.1-flash*
z-ai/glm-5.*-flash
qwen/qwen3.7-flash
tencent/hy4-flash
inclusionai/ling-3.1-flash
nvidia/nemotron-3.5-lightning
minimax/minimax-m3.*-flash
```

El orden de la lista es meramente declarativo: la clasificación ordena por
`blend` de precio, no por la posición en `allow`. Editarla no requiere
recompilar.

## Variables de entorno

- `OPENROUTER_API_KEY` — clave de OpenRouter, referenciada por `api_key_env`. La
  usa tanto el backend dinámico como el de pago. Debe estar presente en
  `~/.config/opencode/llm-gateway.env`.
- `GATEWAY_CONFIG` — ruta alternativa del fichero de configuración (por defecto
  `gateway.yaml`).

El descubrimiento del catálogo (`GET /models`) es público y **no** usa la clave.

## Cómo habilitar / desplegar

1. Añadir las rutas a `~/.config/opencode/gateway.yaml` (el fichero de runtime no
   vive en el repo; el ejemplo es `gateway.yaml.example`).
2. Reiniciar el servicio: `systemctl --user restart llm-gateway`.
3. Verificar las métricas de descubrimiento:
   `curl -s http://127.0.0.1:4010/metrics | rg llm_gateway_discovery`.

## Verificación

- `curl -s http://127.0.0.1:4010/v1/models | rg -e architect -e code -e writer`
  incluye las rutas y los modelos dinámicos elegibles.
- `curl -s http://127.0.0.1:4010/metrics | rg llm_gateway_discovery` muestra
  `llm_gateway_discovery_models`, `llm_gateway_discovery_free_models`,
  `llm_gateway_discovery_profile_models`, `llm_gateway_discovery_refresh_total`
  y `llm_gateway_discovery_blacklist`.
- `curl -s http://127.0.0.1:4010/metrics | rg free_quota` muestra el gauge
  `llm_gateway_free_quota_remaining` y el counter
  `llm_gateway_free_quota_skips_total` (el gauge solo aparece tras un sondeo o
  una observación).
- Con la cuota diaria agotada, la inferencia la sirve el modelo **más barato**
  de la lista curada del backend `openrouter_paid` (la cabecera
  `x-gateway-backend` lo identifica como `<base_url>#<modelo>`), sin intentos
  free y con el contador de saltos incrementando.
- Inferencia real a través del gateway con `finish_reason: stop`.
- Failover: con el tier free agotado (gate abierto, todos en blacklist o
  fallando), responde el respaldo de pago.

## Decisiones

Resumen de las decisiones de `design.md`:

- **Razonamiento primero en `architect`**, con general grande como respaldo, para
  evitar que la ruta quede vacía cuando no hay familias de razonamiento free con
  tool-calling.
- **Un solo descubrimiento, tres clasificaciones**: un único `/models` y un único
  TTL alimentan los tres perfiles (menos tráfico y caches coherentes).
- **`profile` a nivel de `Backend`**, no de ruta, para poder mezclar perfiles y
  declarar fallbacks de pago concretos por perfil.
- **Blacklist global por modelo**: un `429` es por combinación modelo+clave, así
  que la exclusión es transversal a las rutas.
- **Reutilizar el motor de failover existente**: el backend dinámico se expande a
  modelos concretos y `route_with_failover` hace el resto, sin duplicar lógica.
- **Refresco en segundo plano con `stale-while-error`**: el TTL no bloquea la
  petición y un fallo conserva la caché previa.
- **Gate global de cuota diaria**: el agotamiento es de la clave, no del modelo;
  abrir un único gate evita cadenas de `429` y salta directo al respaldo.
- **Lista curada en YAML, no en el código**: `discovery.allow` permite cambiar la
  lista de modelos de pago sin recompilar; el código no impone ninguna por
  defecto (un `allow` ausente aporta cero candidatos).
- **Selección pegajosa del modelo de pago**: cambiar de modelo a cada refresco de
  precio añade varianza y coste de *cold start*; la cadencia + histéresis
  (`reselect_interval_ms`, `min_price_delta`) mantiene estabilidad y solo migra
  ante una mejora clara, salvo que el modelo elegido caiga.
- **Un solo catálogo para free y pago**: mismo `/models` y mismo TTL por
  `base_url`, lo que garantiza que la lista curada se evalúe contra el catálogo
  ya cacheado, sin tráfico extra.

## Follow-ups

Hallazgos no bloqueantes detectados en la implementación inicial. M5, M6, N1,
N2, N3 y N4 se cerraron en el change `add-openrouter-free-followups`; ver
[`docs/openrouter-free-followups.md`](openrouter-free-followups.md). Se
conserva el histórico:

- **M5 — clones de `Backend` en el hot path.** `expand_backends` clona el
  `Backend` una vez por modelo expandido (y también los estáticos), en cada
  petición. Optimizable reutilizando una plantilla o un `Arc`.
- **M6 — TTL por `base_url` gana el primer backend.** El registro de catálogos se
  indexa por `base_url`; si dos backends comparten `base_url` con distinto
  `refresh_interval_ms`, prevalece el del primero encontrado. Convendría validar
  la coherencia del TTL por `base_url` o rechazarla en configuración.
- **N1 — `Discovery` sin defaults por campo (`src/config.rs`, ~L172-182).**
  `refresh_interval_ms` y `blacklist_cooldown_ms` son obligatorios si el bloque
  `discovery` está presente, así que `discovery: { min_context: 32768 }` falla al
  deserializar. Fix propuesto: `#[serde(default = "...")]` por campo.
- **N2 — `include` no elegible descartado en silencio (`src/classify.rs`,
  ~L46-53).** Un `include` que nombra un modelo no elegible se ignora sin
  log/warn; conviene avisar al operador.
- **N3 — deduplicación/búsqueda lineal (`src/discovery.rs`).**
  `eligible_model_ids` deduplica con `Vec::contains` (O(n²), ~L498-520) y
  `Registry::dynamic` hace búsqueda lineal (~L449-453). Irrelevante con N
  pequeño; anotado por completitud.
- **N4 — `set_var` bajo tests paralelos (`tests/dynamic_routing.rs`, ~L179).**
  `unsafe { std::env::set_var(...) }` convive con tests que leen el entorno
  (`resolve_api_key`) → carrera `set_var`/`getenv` (UB documentada por std).
  **Patrón preexistente** del repo (también en `tests/secrets.rs`,
  `tests/warmup.rs`), no introducido por esta feature; mitigación: fijar la
  variable una sola vez o serializar los tests.
