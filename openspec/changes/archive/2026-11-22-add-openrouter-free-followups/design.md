# Design

## Context

Ver `proposal.md` — Why. El estado actual relevante:

- `src/proxy.rs` define `Candidate { slot, backend: Backend, dynamic_cooldown }` y `expand_backends` clona el `Backend` completo por cada modelo expandido y en cada petición.
- `src/discovery.rs` agrupa catálogos por `base_url`; el TTL lo fija el primer backend dinámico (`or_insert_with`) y `Registry::dynamic` recorre `Vec<DynamicBackend>` en lineal. `Blacklist::purge_and_report` escribe el gauge en cada acceso.
- `src/config.rs` define `Discovery { refresh_interval_ms, blacklist_cooldown_ms, min_context, overrides }` sin defaults por campo.
- `src/classify.rs` descarta en silencio los `include` que no son elegibles.
- `tests/dynamic_routing.rs` usa `std::env::set_var` dentro de tests que corren en paralelo con lecturas de entorno de otros tests.

## Goals / Non-Goals

**Goals**

- Eliminar el clon del `Backend` completo del hot path sin cambiar valores enviados al upstream.
- Hacer explícita y validada la coherencia de TTL por `base_url`.
- Permitir bloques `discovery` parciales con defaults seguros.
- Observar los `include` descartados sin cambiar el resultado de clasificación.
- Reducir escrituras de gauge y coste cuadrático en `eligible_model_ids`.
- Hacer deterministas los tests que tocan variables de entorno.

**Non-Goals**

- No se rediseña el descubrimiento ni la heurística de perfiles.
- No se añaden dependencias nuevas.
- No se cambia el esquema de métricas expuesto ni los endpoints.

## Decisions

### D1 — `Candidate` ligero

`Candidate` pasa a contener `slot: usize`, `model: String`, `base_url: String`, `timeout_ms: Option<u64>`, `api_key_env: Option<String>`, `dynamic_cooldown: Option<Duration>`. La función que construye la petición recibe `&Candidate`. `blacklist_if_needed` recibe `model: &str` en lugar de `&Backend`.

*Alternativa descartada:* clonar solo `Backend` sin `discovery`/`overrides`/`warmup` (sigue arrastrando campos innecesarios y obliga a un tipo intermedio).

### D2 — Índice `(route, backend)`

`Registry` mantiene `dynamic: Vec<DynamicBackend>` para el arranque y añade `index: HashMap<(usize, usize), usize>` con la posición en ese `Vec`. `Registry::dynamic` pasa a un `get` O(1). Si se prefiere menos duplicación, alternativa válida: `Vec<Vec<usize>>` por ruta. Se elige el `HashMap` por claridad y porque el número de backends dinámicos es pequeño.

### D3 — Deduplicación estable

`eligible_model_ids` usa `HashSet<String>` para deduplicar preservando el orden de primera aparición (se consulta antes de insertar). El orden externo se mantiene estable entre llamadas con el mismo catálogo.

### D4 — Gauge con último valor publicado

La guarda vive en `Metrics` (`blacklist_size: AtomicUsize`, inicializado a 0, que es lo que ya publica `Metrics::new`), no en `Blacklist`. Motivo: `Metrics` es la única fuente de verdad del gauge y el punto por el que pasa cualquier escritura; así la política «escribir solo si cambia» se aplica una sola vez y no depende de que todos los accesos a la blacklist la recuerden. `set_blacklist_size` hace `swap` y solo llama a `record` si el tamaño difiere del último publicado; `Blacklist::purge_and_report` conserva su firma y su comportamiento externo y sigue siendo quien calcula el tamaño tras purgar. Las llamadas desde `Blacklist` ocurren bajo su lock de escritura: bajo ese lock no hay valor incorrecto, pero la secuencia `swap` + `record` requiere serialización externa y no es segura por sí sola. Además, la caché del último valor es **por instancia de `Metrics`** y asume una sola instancia por proceso, porque la serie OTel es global.

### D5 — TTL por `base_url` validado

La validación de config construye un mapa `base_url -> (ttl, rutas)` recorriendo los backends dinámicos. TTL efectivo = `discovery.refresh_interval_ms` o el default. Un segundo TTL distinto para la misma `base_url` produce error con la `base_url` y el `name` de las rutas implicadas. `Registry::new` puede entonces usar `or_insert_with` sin ambigüedad, y se documenta que el catálogo es compartido. La validación vive en `GatewayConfig::validate`, no en `Registry::new`, que solo se beneficia de ella. El error es *fail-fast*: se aborta en el primer conflicto detectado y el mensaje lista las rutas vistas hasta ese punto (comportamiento intencional).

### D6 — Defaults de `Discovery`

`#[serde(default = "default_refresh_interval_ms")]` y `#[serde(default = "default_blacklist_cooldown_ms")]` reutilizan las constantes existentes (`DEFAULT_REFRESH_INTERVAL_MS`, `DEFAULT_BLACKLIST_COOLDOWN_MS`). La validación de `0` se mantiene y se aplica también a los valores por defecto solo si el usuario los declara (los defaults son > 0).

### D7 — Aviso de `include` ignorado

`classify` recibe un contexto mínimo (`base_url: &str` y `profile`) o un struct `ClassifyContext`. Para cada `include` no elegible emite `tracing::warn!(model, profile, base_url, "include ignorado: modelo no elegible")`. La salida no cambia.

### D8 — Tests de entorno deterministas

Helper con `OnceLock` en `tests/dynamic_routing.rs` que fija las variables una sola vez antes de cualquier test, evitando `set_var` repetido. Si alguna variable debe variar por test, se usa un `Mutex` global que serializa solo esos tests.

## Risks / Trade-offs

- **TTL incoherente ahora es error:** configs que hoy funcionan (ignorando el segundo TTL) dejarán de cargar. Mitigación: mensaje explícito con `base_url` y rutas, y nota en `docs/`.
- **Cambio de firmas interno:** `classify` y `Candidate` afectan a varios call sites; los tests existentes de proxy y discovery cubren el contrato observable.
- **Cache del gauge:** bajo el lock de `Blacklist` no hay valor incorrecto; sin esa serialización externa, dos escrituras concurrentes pueden dejar una escritura redundante o la caché desfasada. La caché asume además una sola instancia de `Metrics` por proceso.
- **N4:** serializar tests reduce paralelismo solo en `dynamic_routing`.

## Migration

No hay migración de datos ni de configuración obligatoria: los defaults preservan el comportamiento. La única ruptura es la validación de TTL incoherente, documentada como error explícito.