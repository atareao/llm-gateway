# Rutas dinámicas OpenRouter free por perfil

## Qué es

El gateway incorpora **rutas dinámicas de modelos gratuitos de OpenRouter**,
especializadas por **perfil de uso** (`architect`, `code`, `writer`). En lugar de
fijar un modelo concreto en la configuración, un backend de tipo
`openrouter_free` descubre el catálogo público de OpenRouter, filtra los modelos
gratuitos que admiten *tool-calling*, los clasifica según el perfil y los usa
como cadena de candidatos con **failover**, **rotación round-robin** y
**blacklist dinámica**, cayendo a un backend de **pago** declarado por ruta
cuando el tier free se agota.

Es opt-in: las configuraciones existentes siguen funcionando sin cambios
(`kind` por defecto es `static`).

## Ficheros implicados

- `src/config.rs` — `BackendKind`, `Profile`, `Discovery` y `ClassifyOverrides`;
  defaults (`DEFAULT_REFRESH_INTERVAL_MS`, `DEFAULT_BLACKLIST_COOLDOWN_MS`) y
  validación semántica.
- `src/discovery.rs` — consulta `GET {base_url}/models`, filtro free +
  tool-calling, caché con TTL, refresco en segundo plano (`stale-while-error`) y
  blacklist global.
- `src/classify.rs` — heurística de clasificación por perfil, `min_context` y
  overrides.
- `src/proxy.rs` — expansión de backends dinámicos en su posición, rotación,
  blacklist al recibir `429`/`5xx`/`Retry-After`, fallback y `AllBackendsFailed`.
- `src/metrics.rs` — series `llm_gateway_discovery_*`.

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
free se agota (todo en blacklist o fallando), se usa el backend de **pago**
declarado a continuación. Si se agota la ruta completa, el gateway responde con
`AllBackendsFailed`.

## Configuración

Campos nuevos del `Backend` y sus valores por defecto reales:

| Campo | Tipo | Por defecto | Descripción |
|---|---|---|---|
| `kind` | `static` \| `openrouter_free` | `static` | Backend fijo o dinámico. |
| `profile` | `architect` \| `code` \| `writer` | `code` | Perfil de clasificación del backend dinámico. |
| `discovery` | bloque | ausente | Configuración del descubrimiento (opcional). |
| `discovery.refresh_interval_ms` | `u64` | `300000` (5 min) | TTL de la caché y periodo de refresco. |
| `discovery.blacklist_cooldown_ms` | `u64` | `60000` (1 min) | Cooldown de la blacklist. |
| `discovery.min_context` | `u32` | sin mínimo (`None`) | Contexto mínimo exigido a los modelos. |
| `discovery.overrides.exclude` | lista | vacía | Modelos excluidos del perfil. |
| `discovery.overrides.include` | lista | vacía | Modelos antepuestos en el perfil. |

Notas:

- Un backend `openrouter_free` requiere `base_url` y `api_key_env`; su `model`
  se ignora (lo resuelve el descubrimiento). Un `static` requiere `base_url` y
  `model`.
- El bloque `discovery` es opcional. Cada campo usa su propio default si se
  omite (`refresh_interval_ms: 300000`, `blacklist_cooldown_ms: 60000`,
  `min_context: None`, `overrides: None`), así que `discovery: { min_context:
  32768 }` es válido. Un `0` explícito en los TTL sigue siendo error.
- El catálogo se comparte por `base_url`: todos los backends `openrouter_free`
  que usan la misma `base_url` deben declarar el **mismo TTL efectivo**
  (explícito o por default), en la misma ruta o en rutas distintas. Un TTL
  incoherente hace fallar la carga con un error que nombra la `base_url` y las
  rutas implicadas (fail-fast). Detalle en
  [`docs/openrouter-free-followups.md`](openrouter-free-followups.md).
- El timeout de la consulta del catálogo (`CATALOG_TIMEOUT`, 10 s) es una
  constante del código, no configurable.

Ejemplo de las tres rutas (dinámica + respaldo de pago por perfil):

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
      - base_url: "https://openrouter.ai/api/v1"   # fallback de PAGO (razonamiento)
        model: "<REEMPLAZA-modelo-pago-razonamiento>"
        api_key_env: "OPENROUTER_API_KEY"

  - name: code
    backend:
      - kind: openrouter_free
        profile: code
        base_url: "https://openrouter.ai/api/v1"
        api_key_env: "OPENROUTER_API_KEY"
        discovery:
          refresh_interval_ms: 300000
          blacklist_cooldown_ms: 60000
          min_context: 32768
      - base_url: "https://openrouter.ai/api/v1"   # fallback de PAGO (coder)
        model: "<REEMPLAZA-modelo-pago-coder>"
        api_key_env: "OPENROUTER_API_KEY"

  - name: writer
    backend:
      - kind: openrouter_free
        profile: writer
        base_url: "https://openrouter.ai/api/v1"
        api_key_env: "OPENROUTER_API_KEY"
        discovery:
          refresh_interval_ms: 300000
          blacklist_cooldown_ms: 60000
          min_context: 32768
      - base_url: "https://openrouter.ai/api/v1"   # fallback de PAGO (prosa)
        model: "<REEMPLAZA-modelo-pago-prosa>"
        api_key_env: "OPENROUTER_API_KEY"
```

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
- Inferencia real a través del gateway con `finish_reason: stop`.
- Failover: con el tier free agotado (todos en blacklist o fallando), responde el
  backend de pago.

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
