# Proposal

## Why

El gateway ya exporta OTLP a OpenObserve (métricas, logs y trazas), pero **no registra** de forma consultable: (a) la evolución de precios de los modelos del catálogo, (b) qué modelo se eligió en cada ruta y por qué cambió, ni (c) los resultados por petición (tokens, coste, latencia, `finish_reason`). El catálogo `/models` cambia a diario (315 altas en 2026) y un respaldo de pago dinámico puede cambiar de modelo sin que quede rastro de por qué. Además, hoy el gateway **no parsea `usage`** en ningún punto, así que no hay coste real medible.

## What Changes

- **Parseo de `usage`**: extraer `prompt_tokens` y `completion_tokens` de la respuesta de OpenRouter (no-stream: se lee el body; stream: inyectar `stream_options.include_usage: true` y leer el chunk final con `usage`).
- **Tres eventos de negocio** emitidos como log estructurado, consultables en OpenObserve:
  - `gateway_catalog` — **solo al cambiar** un precio o al aparecer/desaparecer un modelo (dedup en memoria contra el último snapshot).
  - `gateway_selection` — **solo al cambiar** el modelo pegajoso de un `(route, backend)`.
  - `gateway_requests` — **una por petición**, denormalizada con `model_id`, precios y `cost_usd` calculado.
- **Métricas nuevas**: tokens, coste, precio del modelo seleccionado, precio de la allowlist y contador de selecciones.
- **Retención** documentada por evento, configurada en OpenObserve (no en el gateway).
- No **BREAKING**: la emisión es opt-out (`observability.events`, default `true`).

## Capabilities

### New Capabilities

- `observability`: registro de eventos de negocio (catálogo, selección, peticiones) y métricas de tokens y coste.

### Modified Capabilities

(ninguna)

## Impact

- **Código**: `src/proxy.rs` (parsear `usage`, calcular `cost_usd`, emitir el evento de petición), `src/discovery.rs` (emitir el evento de catálogo al cambiar el precio y el de selección al cambiar el modelo), `src/telemetry.rs` (nombre/atributos de los eventos), `src/metrics.rs` (métricas nuevas), `src/config.rs` (`observability.events`).
- **Config**: nuevo bloque `observability` con `events: bool` (default `true`).
- **Dependencia**: consume los eventos de selección y de precio introducidos por #37 (`add-openrouter-free-quota-guard`), pero es ortogonal.
- **Docs**: `docs/observability-ledger.md` (nuevo) con streams, campos, retención y consultas de ejemplo.
- **Issue**: #39.
