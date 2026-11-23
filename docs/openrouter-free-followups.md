# Follow-ups del enrutado dinámico OpenRouter free

## Qué es

Cierre de los follow-ups del issue #32 sobre la feature de **rutas dinámicas
OpenRouter free** (`openrouter_free`). El change
`openspec/changes/add-openrouter-free-followups/` recoge los hallazgos no
bloqueantes de la auditoría de `add-openrouter-free-routing`: clones en el hot
path, TTL por `base_url`, defaults de `discovery`, aviso de `include` ignorado,
coste cuadrático de la deduplicación, escrituras redundantes del gauge de
blacklist y una carrera de entorno en los tests.

No cambia endpoints, esquema de configuración ni nombres de métricas. La única
ruptura es que una configuración con TTL incoherente por `base_url` ahora falla
al cargar (antes prevalecía el primer TTL visto).

## Ficheros implicados

- `src/proxy.rs` — `Candidate` ligero, `format_id` y construcción de la
  petición sin clonar el `Backend` completo.
- `src/discovery.rs` — índice `(route, backend)`, deduplicación con `HashSet`,
  guarda del gauge en `Metrics` y escenarios de test.
- `src/config.rs` — defaults por campo de `Discovery`, validación de TTL único
  por `base_url` y `format_id` como productor único del identificador.
- `src/classify.rs` — `ClassifyContext` (perfil, `min_context`, `overrides`,
  `base_url`) y `ignored_includes` para avisar de los `include` no elegibles.
- `src/metrics.rs` — guarda del gauge (`blacklist_size`), contrato de
  `set_blacklist_size` y guardián de test del gauge global.
- `tests/dynamic_routing.rs` — `OnceLock` para la API key de test, contadores
  del mock de `/v1/models` y escenarios de TTL vencido/fallo.

## Cómo funciona

### `Candidate` ligero (D1)

`Candidate` solo arrastra lo que el enrutado necesita: `slot` (índice del
backend para métricas), `model` (el real, ya expandido si es dinámico),
`base_url`, `timeout_ms`, `api_key_env` y `dynamic_cooldown` (solo en
candidatos procedentes de un backend dinámico). `expand_backends` construye los
candidatos campo a campo con `Candidate::from_backend`; el `Backend` completo
con su `discovery`, `warmup` y `overrides` no se clona por petición. El
identificador `<base_url>#<model>` se centraliza en `config::format_id`, de modo
que `Backend::label`, `proxy::backend_id` y `Candidate::id` no divergen.

### Índice `(route, backend)` (D2)

`Registry` conserva `dynamic: Vec<DynamicBackend>` para el arranque y añade
`index: HashMap<(usize, usize), usize>` con la posición en ese vector. Así
`Registry::dynamic(route, backend)` resuelve el cooldown en O(1) en lugar de
recorrer todos los backends dinámicos. Los slots estáticos no están en el índice
y devuelven `None`.

### Deduplicación estable (D3)

`eligible_model_ids` deduplica con `HashSet<String>`, consultando el conjunto
antes de insertar: se conserva el orden de primera aparición entre backends y
entre llamadas con el mismo catálogo. El resultado sigue siendo determinista.

### Guarda del gauge (D4)

`Metrics` guarda el último tamaño publicado en `blacklist_size: AtomicUsize`
(inicializado a 0, que es lo que deja el pre-registro) y
`set_blacklist_size` hace `swap`; solo llama a `record` si el tamaño difiere.
`Blacklist::purge_and_report` mantiene su firma y sigue calculando el tamaño tras
purgar, pero los accesos sin cambio de tamaño ya no reescriben el gauge.

La secuencia `swap` + `record` exige serialización externa: las llamadas desde
`Blacklist` quedan serializadas por su lock de escritura. La caché es **por
instancia de `Metrics`** y asume una única instancia por proceso, porque la
serie OTel del gauge es global. En tests, `BlacklistGaugeGuard` serializa los
tests que leen o escriben el gauge y permite que un `Metrics::new` dentro de la
sección crítica pre-registre el 0 sin bloquearse.

### Defaults y validación de TTL (D5, D6)

`Discovery.refresh_interval_ms` y `Discovery.blacklist_cooldown_ms` tienen
`#[serde(default = ...)]`; los valores reales son `300000` ms (5 min) y `60000`
ms (1 min). `min_context` y `overrides` siguen siendo opcionales. Un `0`
explícito sigue siendo error de configuración.

El catálogo se comparte por `base_url`, así que todos los backends dinámicos que
la comparten deben usar el **mismo TTL efectivo**. `GatewayConfig::validate`
agrupa por `base_url` (TTL explícito o default) y aborta en el primer conflicto,
con un error que incluye la `base_url`, el TTL esperado, el encontrado y las
rutas implicadas.

### Aviso de `include` ignorado (D7)

`classify` recibe un `ClassifyContext` con `profile`, `min_context`,
`overrides` y `base_url`. Cada `include` que no es elegible (ausente del
catálogo, filtrado por `min_context` o excluido por `exclude`) se descarta y
emite `tracing::warn!` con `model`, `profile` y `base_url`. La lista resultante
no cambia. `ignored_includes` deduplica los avisos por identificador.

### Tests deterministas (D8)

`tests/dynamic_routing.rs` fija la API key de test una sola vez por proceso con
un `OnceLock` antes de construir el primer `AppState`. La soundness de
`std::env::set_var` viene del orden de acceso (todo lector pasa antes por
`ensure_test_api_key()`), no solo de la unicidad de la escritura.

## Variables/config

Bloque `discovery` completo:

```yaml
route:
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
          overrides:
            include: ["deepseek/deepseek-r1:free"]
            exclude: ["some/model:free"]
```

Bloque `discovery` parcial: los campos omitidos toman su default.

```yaml
        discovery:
          min_context: 32768
```

También es válido omitir `discovery` por completo; equivale a

```yaml
        discovery:
          refresh_interval_ms: 300000
          blacklist_cooldown_ms: 60000
```

Configuraciones que **dejan de cargar**:

- `discovery.refresh_interval_ms: 0` o `blacklist_cooldown_ms: 0` (error
  explícito).
- Dos o más backends dinámicos con la misma `base_url` y distinto TTL efectivo
  (explícito o por default), en la misma ruta o en rutas distintas.

## Cómo habilitar / despliegue

No requiere cambios de despliegue ni migración: un `gateway.yaml` existente que
ya cargaba sigue siendo válido mientras los TTL por `base_url` sean coherentes.
Los defaults preservan el comportamiento anterior.

Verificación local:

```bash
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
```

No existe hoy un flag `--check-config`; para validar un fichero se arranca el
binario con `GATEWAY_CONFIG` (el arranque valida antes de escuchar) o se carga
con `llm_gateway_rs::config::load` en un test.

## Decisiones

- **D1 — `Candidate` ligero.** Copiar solo los campos usados evita clonar
  `discovery`, `warmup` y `overrides` en cada petición. Alternativa descartada:
  clonar el `Backend` sin esos campos, que obliga a un tipo intermedio.
- **D2 — Índice `(route, backend)`.** `HashMap` por claridad; el número de
  backends dinámicos es pequeño y el acceso pasa de lineal a O(1).
- **D3 — Deduplicación estable.** `HashSet` con consulta previa a la inserción
  conserva el orden de primera aparición y elimina el coste O(n²).
- **D4 — Gauge en `Metrics`.** La política «escribir solo si cambia» vive en el
  único punto por el que pasan todas las escrituras, no en cada llamante de la
  blacklist. Requiere serialización externa (lock de `Blacklist`) y una sola
  instancia de `Metrics` por proceso.
- **D5 — TTL validado fail-fast.** Rechazar en carga evita que el primer
  backend decida el TTL de todos por accidente; el mensaje nombra la `base_url`
  y las rutas.
- **D6 — Defaults de `Discovery`.** Permiten bloques parciales sin duplicar
  constantes; los defaults son mayores que 0 y la validación de `0` explícito
  se mantiene.
- **D7 — Aviso de `include` ignorado.** Observabilidad sin cambiar la salida de
  clasificación; se emite una vez por identificador.
- **D8 — `OnceLock` en tests.** Evita `set_var` repetido y fija el orden de
  acceso; si un test necesitara valores distintos, se serializaría con un
  `Mutex` global.

## Follow-ups

- **`Arc<str>` para `base_url`/`api_key_env` por candidato.** Hoy se clonan
  `String` en cada `Candidate`; con `Arc<str>` se compartirían sin asignación.
- **Deduplicar el warning de `include` entre peticiones.** `ignored_includes`
  deduplica dentro de una clasificación, pero cada petición vuelve a emitir el
  mismo aviso mientras el `include` siga sin ser elegible.
- **Caché del gauge si un embedder crea varias instancias de `Metrics`.**
  `blacklist_size` es por instancia y la serie OTel es global; varias
  instancias en el mismo proceso podrían desincronizar la caché del valor
  publicado. En el binario hay una sola instancia.

## Cierre del milestone B — respaldo de pago dinámico (#37)

Cierre del **milestone B** (respaldo de pago dinámico) del change
`openspec/changes/add-openrouter-free-quota-guard/` (issue #37). No cambia
endpoints ni esquema: añade un `kind: openrouter_paid` opcional y campos
opcionales en `discovery`. La verificación E2E manual queda para el
coordinador.

### Qué se ha cerrado

- **Gate de cuota free, reactivo y proactivo.** El `429` de cuota diaria
  (`limit_source == openrouter_free_tier_daily`, con heurística
  `X-RateLimit-Remaining: 0` + `X-RateLimit-Reset` > 5 min) abre un gate
  global con deadline `T` (epoch de `X-RateLimit-Reset`, fallback al siguiente
  `00:00 UTC`, tope `MAX_QUOTA_GATE` de 24 h). En paralelo, el sondeo de
  `GET {base_url}/key` abre el mismo gate cuando
  `free_model_daily_requests.remaining == 0`. Con el gate abierto la expansión
  dinámica free devuelve cero candidatos y la ruta salta al respaldo.
- **`kind: openrouter_paid` con lista curada `discovery.allow`.** Un backend de
  pago reutiliza el catálogo cacheado (mismo `/models` y mismo TTL por
  `base_url`, sin tráfico extra) y propone únicamente modelos que coinciden con
  algún patrón glob de `allow`, soportan tool-calling, tienen precio positivo
  (excluye el tier free y los `:batch`) y cumplen `min_context`. Un `allow`
  ausente o vacío aporta cero candidatos; un patrón válido sin coincidencias se
  registra como aviso, no como error.
- **Más barato de la allowlist por `blend`.** Los elegibles se ordenan por
  `blend = 0.75·entrada + 0.25·salida` ascendente, con desempate por
  `context_length` desc y `id` asc. El primero es el modelo elegido y el resto
  el orden de failover, **sin round-robin** (a diferencia de `openrouter_free`).
- **Selección pegajosa con histéresis.** El modelo elegido por `(route, backend)`
  solo cambia si falla o sale del catálogo, o si ha pasado
  `reselect_interval_ms` (default 4 h) **y** el más barato mejora el precio al
  menos `min_price_delta` (default 0.20). El refresco del catálogo no cambia el
  modelo por sí solo; el estado no persiste entre reinicios.
- **`static` final opcional.** `gateway.yaml.example` conserva un `kind: static`
  como último recurso tras el `openrouter_paid`.

### Follow-ups vivos

- **Deduplicar el alias `z-ai/glm-flash-latest` ≈ `z-ai/glm-5.3-flash`.** El
  catálogo de OpenRouter publica alias que apuntan al mismo modelo; con
  `allow: ["z-ai/glm-5.*-flash"]` pueden aparecer dos entradas equivalentes y
  ocupar dos posiciones del failover.
- **Permitir `allow` por backend/ruta si algún día se quiere afinar por
  perfil.** Hoy `allow` es global al bloque `discovery`; si los perfiles
  necesitaran listas curadas distintas, habría que moverlo a nivel de backend.
- **Histórico de precios y coste por petición: change aparte
  `add-observability-ledger` (issue #39).** La selección por precio actual no
  guarda serie temporal ni imputa coste por petición; eso vive en ese change.
