# Design

## Context

Ver `proposal.md` — Why. Estado actual relevante:

- `src/proxy.rs::blacklist_if_needed` (~L369) marca en blacklist ante `is_retryable(status)` (`429`/`5xx`) o `Retry-After`, con `effective_cooldown` (~L392) = `max(config, Retry-After)` **capado a `MAX_RETRY_AFTER` = 1 h** (~L76).
- `src/discovery.rs::Blacklist` guarda expiración por modelo; `Registry::dynamic` (~L560) filtra por blacklist y aplica rotación round-robin.
- `Registry::spawn_refresh` (~L622) ya refresca el catálogo en background siguiendo el patrón de `src/warmup.rs`.
- OpenRouter documenta que un `429` de cuota diaria free trae `X-RateLimit-Limit`, `X-RateLimit-Remaining`, `X-RateLimit-Reset` (epoch en ms) y, en el cuerpo, `error.metadata.limit_source = "openrouter_free_tier_daily"`. `GET /api/v1/key` expone `free_model_daily_requests.{used,limit,remaining}`, que resetea a las 00:00 UTC.

## Goals / Non-Goals

**Goals**

- Cero intentos free mientras la cuota diaria esté agotada.
- Abrir y cerrar el gate sin intervención manual, con auto-recuperación en el reset.
- Mantener intacto el comportamiento cuando hay cuota disponible.

**Non-Goals**

- No se cambia el modelo de pago en runtime (solo el ejemplo y la doc); el fichero de runtime vive fuera del repo.
- No se implementa backoff adaptativo por minuto: el `429` de minuto sigue con la blacklist actual.
- No se persiste el estado del gate entre reinicios.

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

## Risks / Trade-offs

- [Sondeo extra a `/key`] → limitado por `quota_poll_interval_ms` (60 s) y una llamada por `base_url`; no consume cuota free. Compartido por `base_url`, igual que el catálogo.
- [`limit_source` ausente o cuerpo no-JSON] → se cae a la heurística `X-RateLimit-Remaining: 0` + reset lejano; si tampoco hay cabeceras, no se abre el gate y se mantiene el comportamiento actual.
- [Reloj/epoch] → `X-RateLimit-Reset` viene en ms epoch UTC; se convierte con `SystemTime`. Un valor en el pasado no abre el gate.
- [Gate abierto por error] → tope `MAX_QUOTA_GATE` (24 h) y reapertura de free al vencer; el daño máximo es servir pago durante el resto del día.

## Migration Plan

1. Desplegar la versión con el gate (default `respect_daily_quota: true`).
2. Reiniciar: `systemctl --user restart llm-gateway`.
3. Verificar: `curl -s http://127.0.0.1:4010/metrics | rg free_quota`.
4. Actualizar `~/.config/opencode/gateway.yaml` con el nuevo modelo de respaldo (fuera del repo).
- **Rollback**: `respect_daily_quota: false` o revertir el binario.

## Open Questions

Ninguna.
