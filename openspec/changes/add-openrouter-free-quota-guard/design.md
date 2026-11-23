# Design

## Context

Ver `proposal.md` — Why. Estado actual relevante:

- `src/proxy.rs::blacklist_if_needed` (~L369) marca en blacklist ante `is_retryable(status)` (`429`/`5xx`) o `Retry-After`, con `effective_cooldown` (~L392) = `max(config, Retry-After)` **capado a `MAX_RETRY_AFTER` = 1 h** (~L76).
- `src/discovery.rs::Blacklist` guarda expiración por modelo; `Registry::expand` (~L564) filtra por blacklist, ordena con `classify` (~L576) y aplica rotación round-robin.
- `src/classify.rs::classify` (~L56) ordena por `tier()` de palabras clave del perfil, luego `context_length` desc, luego `id`. `tier()` (~L156) es un matcher de keywords (`"thinking"`, `"coder"`, `"70b"`…), **no** un ranking de calidad.
- `src/discovery.rs::ModelEntry` ya expone `pricing: Option<Pricing>` (~L60) y `context_length`; `is_free` (~L106) distingue el tier free.
- `src/config.rs`: `pub enum BackendKind { Static, OpenrouterFree }` (~L139, `rename_all = "snake_case"`), `pub enum Profile { Architect, Code, Writer }` (~L150) y `pub struct Discovery` (~L183) con `refresh_interval_ms` (default `300000`), `blacklist_cooldown_ms`, `min_context: Option<u32>` y `overrides`.
- `Registry::spawn_refresh` (~L622) ya refresca el catálogo en background (`discovery.refresh_interval_ms`) siguiendo el patrón de `src/warmup.rs`.
- OpenRouter documenta que un `429` de cuota diaria free trae `X-RateLimit-Limit`, `X-RateLimit-Remaining`, `X-RateLimit-Reset` (epoch en ms) y, en el cuerpo, `error.metadata.limit_source = "openrouter_free_tier_daily"`. `GET /api/v1/key` expone `free_model_daily_requests.{used,limit,remaining}`, que resetea a las 00:00 UTC.
- El catálogo `/models` **no trae ninguna métrica de calidad**. Por eso un respaldo de pago no puede apoyarse en `tier()`: la calidad debe venir de una **lista curada**.

## Goals / Non-Goals

**Goals**

- Cero intentos free mientras la cuota diaria esté agotada.
- Abrir y cerrar el gate sin intervención manual, con auto-recuperación en el reset.
- Elegir el modelo de pago **más barato dentro de una lista curada**, actualizable con una línea, sin tocar `gateway.yaml`.
- No cambiar de modelo de pago a cada refresco (preservar la caché de prompt del proveedor y la consistencia del bucle agéntico).
- Mantener intacto el comportamiento cuando hay cuota disponible.

**Non-Goals**

- No se introduce un ranking de calidad heurístico (keywords): la curación la decide el operador.
- No se implementa backoff adaptativo por minuto: el `429` de minuto sigue con la blacklist actual.
- No se persiste el estado del gate ni el de selección entre reinicios.
- No se registra el histórico de precios ni el coste por petición: eso es el change `add-observability-ledger` (issue #39).

## Decisions

### D1: Gate global de cuota como estado del `Registry`, no por modelo

Un `429` de cuota diaria no es un problema del modelo, sino de la cuenta/clave. Por eso el gate es **global** (compartido por todas las rutas y `base_url`), con un único deadline `free_quota_exhausted_until: Option<Instant>`.

- **Alternativa descartada**: blacklistear los 12 modelos individualmente con cooldown hasta el reset. Multiplica estado y no evita que un modelo nuevo del catálogo vuelva a intentarse.

### D2: Deadline desde `X-RateLimit-Reset`, con fallback a 00:00 UTC

Se parsea `X-RateLimit-Reset` (epoch en ms) a `Instant`. Si falta, se calcula el próximo `00:00 UTC`. Se aplica un tope de seguridad `MAX_QUOTA_GATE` (24 h) para no quedar bloqueado por un valor absurdo.

- **Alternativa descartada**: reutilizar `MAX_RETRY_AFTER` (1 h) → insuficiente para un reset diario; reintroduciría el bucle.

### D3: Distinguir el `429` diario del transitorio

Se lee `error.metadata.limit_source` del cuerpo (cuando es JSON y pequeño) y/o se usa la heurística `X-RateLimit-Remaining == 0` junto con `X-RateLimit-Reset` a más de 5 minutos vista. Solo se abre el gate con `openrouter_free_tier_daily` (o la heurística). El `429` de minuto y el de proveedor mantienen la blacklist por modelo sin cambios.

- **Alternativa descartada**: abrir el gate con cualquier `429` → un pico de minuto bloquearía el free todo el día.

### D4: Sondeo proactivo con el patrón de refresco existente

Una tarea de background (patrón `spawn_refresh`) consulta `GET {base_url}/key` cada `quota_poll_interval_ms` (default 60000) con la API key del backend y abre el gate si `remaining == 0`. La lectura de `/key` no consume cuota free y se comparte por `base_url`.

- **Alternativa descartada**: depender solo del reactivo → siempre se paga al menos un `429` por día y proceso.

### D5: Config opt-in y compatible

Campos nuevos en `discovery`: `respect_daily_quota: bool` (default `true`) y `quota_poll_interval_ms: u64` (default `60000`). Ausentes = defaults. `respect_daily_quota: false` desactiva gate y sondeo.

- **Alternativa descartada**: activarlo siempre sin flag → rompería configuraciones que quieran el comportamiento actual.

### D6: Backend de pago dinámico con lista curada y orden por precio

Se añade la variante unitaria `BackendKind::OpenrouterPaid` (serde `openrouter_paid`), simétrica a `OpenrouterFree`, reutilizando el `base_url`, la `api_key`, el `profile` y el `min_context` ya presentes en el backend, y el catálogo cacheado por `base_url` con su TTL (`refresh_interval_ms`).

Elegibilidad de un modelo: coincidir con algún patrón de `discovery.allow` (glob sobre `model_id`) **y** soportar tool-calling (`tools` y `tool_choice`) **y** `pricing > 0` (excluye el tier free) **y** `context_length >= min_context` **y** no ser `:batch`. Orden: `blend = 0.75·pricing.prompt + 0.25·pricing.completion` ascendente, con desempate por `context_length` desc y `id` ascendente. El primer candidato es el elegido; el resto son el orden de failover. **Sin round-robin** (a diferencia de `openrouter_free`).

- **Alternativa descartada**: `selection: cheapest` sobre **todo** el catálogo → elige modelos enanos (`inclusionai/ling-3.0-tiny`, `meta-llama/llama-3.1-8b-instruct`) que en bucle agéntico salen caros por más turnos.
- **Alternativa descartada**: reutilizar `tier()` (keywords) con estrategia "balanced" → metió `qwen/qwen3-32b` (dense 32B, ~40k de contexto real en `top_provider`, el más caro de los tres) por decir "thinking" en la descripción.
- **Implementación**: extender `ClassifyContext` con un modo de orden (`Tier` para free, `Price` para paid) o añadir `classify_paid`; añadir un helper `matches_allow(model_id, allow)`.

### D7: Selección pegajosa con histéresis (refrescar datos ≠ cambiar de modelo)

El catálogo se refresca cada `refresh_interval_ms` (5 min) —barato, sin cuota—, pero el **modelo elegido** no se recalcula a esa cadencia. El `Registry` guarda por `(route, backend)` el modelo elegido, cuándo se eligió y el precio observado. En cada `expand`:

1. Si el modelo pegajoso sigue elegible y no está blacklisted, y **no** ha pasado `reselect_interval_ms` (default 4 h), o el más barato no es al menos `min_price_delta` (default 0.20) más barato → se mantiene al frente.
2. Si el pegajoso falla o desaparece del catálogo → se cambia **de inmediato** al más barato disponible.
3. Si pasó `reselect_interval_ms` **y** el más barato mejora el precio anterior en ≥ `min_price_delta` → se cambia.

Cambiar de modelo tira la caché de prefijo del proveedor y rompe la consistencia del bucle agéntico; por eso "refrescar precios a menudo" y "cambiar de modelo" se separan.

- **Alternativa descartada**: reordenar por precio en cada request → tiraría la caché cada 5 min sin ganancia real (los precios cambian raramente; lo que cambia son las altas de modelos).

## Risks / Trade-offs

- [Sondeo extra a `/key`] → limitado por `quota_poll_interval_ms` (60 s) y una llamada por `base_url`; no consume cuota free. Compartido por `base_url`, igual que el catálogo.
- [`limit_source` ausente o cuerpo no-JSON] → se cae a la heurística `X-RateLimit-Remaining: 0` + reset lejano; si tampoco hay cabeceras, no se abre el gate y se mantiene el comportamiento actual.
- [Reloj/epoch] → `X-RateLimit-Reset` viene en ms epoch UTC; se convierte con `SystemTime`. Un valor en el pasado no abre el gate.
- [Gate abierto por error] → tope `MAX_QUOTA_GATE` (24 h) y reapertura de free al vencer; el daño máximo es servir pago durante el resto del día.
- [Allowlist mal curada] → si queda vacía o sin coincidencias, el backend de pago aporta cero candidatos y la ruta cae al siguiente backend (el `static` final). Un patrón vacío es error de configuración; un patrón sin coincidencias se registra como aviso, no como error.
- [Selección pegajosa y precios] → si el modelo pegajoso sube de precio, no se cambia hasta `reselect_interval_ms`; es deliberado (estabilidad > micro-ahorro).
- [Antigüedad del snapshot] → la elección usa el catálogo cacheado; un modelo recién abaratado entra como mucho un `refresh_interval_ms` después.

## Migration Plan

1. Desplegar la versión con el gate (default `respect_daily_quota: true`) y el nuevo `kind: openrouter_paid`.
2. Actualizar `gateway.yaml.example`; en el runtime (`~/.config/opencode/gateway.yaml`, fuera del repo) sustituir los backends de pago `static` por `kind: openrouter_paid` con `profile`, conservando un `static` final opcional.
3. Reiniciar: `systemctl --user restart llm-gateway`.
4. Verificar: `curl -s http://127.0.0.1:4010/metrics | rg free_quota` y una inferencia real con `finish_reason: stop`.
- **Rollback**: `respect_daily_quota: false` o volver a backends `static`; o revertir el binario.

## Open Questions

Ninguna.