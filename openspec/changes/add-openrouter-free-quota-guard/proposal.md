# Proposal

## Why

Cuando la cuota diaria de modelos free de OpenRouter se agota (`free_model_daily_requests.remaining == 0`), el gateway sigue intentando el tier free en cada petición: el `429` de cuota diaria solo blacklistea el modelo 60 s, muy por debajo del reset real (00:00 UTC), y el cooldown se capa a 1 h. El resultado medido es un bucle de intentos free quemados (975 + 639 + 175 `retryable_status` en ~11 h) antes de caer al pago, con los 12 modelos free blacklisted de forma recurrente. Además, el respaldo de pago del ejemplo ya no usa el modelo más barato disponible.

## What Changes

- **Gate global "free tier agotado hasta T"**: al detectar un `429` de cuota diaria free, el gateway deja de proponer modelos free hasta T (el reset), saltando directo al backend de pago. Mientras el gate está abierto no se emite ningún intento free.
- **Detección reactiva por cabeceras/`limit_source`**: se distingue el `429` de cuota diaria (`openrouter_free_tier_daily`, `X-RateLimit-Remaining: 0`) del de minuto (`openrouter_free_tier_per_minute`) y del de proveedor (`upstream_provider_shared_pool`). Solo el diario abre el gate; los otros mantienen la blacklist por modelo actual.
- **Detección proactiva vía `GET /api/v1/key`**: sondeo periódico que lee `free_model_daily_requests.remaining` y abre el gate en cuanto llega a 0, antes del primer `429`.
- **Métricas nuevas**: gauge de cuota free restante y contador de saltos a pago por cuota agotada.
- **Respaldo de pago**: el ejemplo y la documentación pasan de `deepseek/deepseek-v4.1-flash` a `z-ai/glm-5.3-flash` (más barato en entrada y salida, 1M de contexto).
- No **BREAKING**: con cuota free disponible el comportamiento es idéntico.

## Capabilities

### New Capabilities

(ninguna)

### Modified Capabilities

- `gateway`: se amplía el enrutado dinámico con un gate global de cuota diaria free, se añade el sondeo de cuota y las métricas asociadas, y se actualiza el modelo de respaldo de pago del ejemplo.

## Impact

- **Código**: `src/proxy.rs` (detección del `429` diario, expansión de candidatos), `src/discovery.rs` (`Registry` y gate de cuota), `src/config.rs` (`Discovery.respect_daily_quota`, `quota_poll_interval_ms`), `src/metrics.rs`.
- **Config**: campos nuevos opcionales en `discovery`; `gateway.yaml.example` cambia el modelo de respaldo de pago.
- **Red**: nueva llamada autenticada periódica a `GET {base_url}/key` (opt-in por config).
- **Docs**: `docs/openrouter-free-routing.md`, `docs/openrouter-free-followups.md`.
- **Issue**: #37.
