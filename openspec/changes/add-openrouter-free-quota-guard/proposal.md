# Proposal

## Why

Cuando la cuota diaria de modelos free de OpenRouter se agota (`free_model_daily_requests.remaining == 0`), el gateway sigue intentando el tier free en cada petición: el `429` de cuota diaria solo blacklistea el modelo 60 s, muy por debajo del reset real (00:00 UTC), y el cooldown se capa a 1 h. El resultado medido es un bucle de intentos free quemados (975 + 639 + 175 `retryable_status` en ~11 h) antes de caer al pago, con los 12 modelos free blacklisted de forma recurrente.

Además, el respaldo de pago está fijado a mano (`deepseek/deepseek-v4.1-flash`) y no sigue el mercado: OpenRouter ofrece modelos *flash* de frontera más baratos (p. ej. `z-ai/glm-5.3-flash`, 1M de contexto, más barato en entrada y salida) y el catálogo cambia a diario (315 altas en 2026). Un modelo de pago fijo se queda obsoleto o caro sin que nadie lo note.

## What Changes

- **Gate global "free tier agotado hasta T"**: al detectar un `429` de cuota diaria free, el gateway deja de proponer modelos free hasta T (el reset), saltando directo al backend de pago. Mientras el gate está abierto no se emite ningún intento free.
- **Detección reactiva por cabeceras/`limit_source`**: se distingue el `429` de cuota diaria (`openrouter_free_tier_daily`, `X-RateLimit-Remaining: 0`) del de minuto (`openrouter_free_tier_per_minute`) y del de proveedor (`upstream_provider_shared_pool`). Solo el diario abre el gate; los otros mantienen la blacklist por modelo actual.
- **Detección proactiva vía `GET /api/v1/key`**: sondeo periódico que lee `free_model_daily_requests.remaining` y abre el gate en cuanto llega a 0, antes del primer `429`.
- **Métricas nuevas**: gauge de cuota free restante y contador de saltos a pago por cuota agotada.
- **Respaldo de pago dinámico (`kind: openrouter_paid`)**: un backend de pago que descubre el catálogo, lo filtra por una **lista curada de patrones** (`discovery.allow`), exige tool-calling y `pricing > 0` (excluye el tier free y los `:batch`), y **ordena por precio ascendente** (`blend = 0.75·entrada + 0.25·salida`). El primer candidato es el más barato de la allowlist; el resto son failover, también en orden de precio. **Sin round-robin**.
- **Selección pegajosa con histéresis**: el modelo elegido se mantiene; solo cambia si falla o desaparece del catálogo, o si ha pasado `reselect_interval_ms` (default 4 h) **y** el candidato más barato es al menos `min_price_delta` (default 0.20) más barato. Preserva la caché de prompt del proveedor y la consistencia del bucle agéntico.
- **Red de seguridad opcional**: el ejemplo puede conservar un backend `static` final como último recurso tras el dinámico de pago.
- No **BREAKING**: con cuota free disponible el comportamiento es idéntico; `static` y `openrouter_free` no cambian.

## Capabilities

### New Capabilities

(ninguna)

### Modified Capabilities

- `gateway`: se amplía el enrutado dinámico con un gate global de cuota diaria free, se añade el sondeo de cuota y las métricas asociadas, y se añade un backend de pago dinámico con lista curada y selección pegajosa por precio.

## Impact

- **Código**: `src/proxy.rs` (detección del `429` diario, expansión de candidatos, selección pegajosa), `src/discovery.rs` (`Registry`, gate de cuota, estado de selección), `src/classify.rs` (orden por precio además del orden por tier), `src/config.rs` (variante `BackendKind::OpenrouterPaid` y campos nuevos en `discovery`), `src/metrics.rs`.
- **Config**: campos nuevos opcionales en `discovery` (`respect_daily_quota`, `quota_poll_interval_ms`, `allow`, `reselect_interval_ms`, `min_price_delta`) y nuevo `kind: openrouter_paid`; `gateway.yaml.example` cambia el respaldo de pago estático por el dinámico (con `static` final opcional).
- **Red**: llamada autenticada periódica a `GET {base_url}/key` (opt-in por config) y reutilización del catálogo `GET /models` ya existente.
- **Docs**: `docs/openrouter-free-routing.md`, `docs/openrouter-free-followups.md`.
- **Issue**: #37.