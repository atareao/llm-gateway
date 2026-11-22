# Proposal

## Why

El catálogo de modelos gratuitos de OpenRouter es amplio pero cambiante y de calidad desigual. Una sola ruta dinámica mezcla modelos de razonamiento, de código y de prosa, de modo que tareas muy distintas acaban en el mismo pool y el round-robin envía trabajo de arquitectura a un modelo flojo (o coding a un modelo de prosa). Se necesitan rutas free especializadas por perfil para cubrir coding y asistencia sin coste, con degradación controlada a pago.

## What Changes

- Nuevo `kind: openrouter_free` en `Backend`, con `profile: architect | code | writer` (default `code`); mezclable con la lista fija de backends. El `kind` por defecto sigue siendo `static`, así que las configuraciones existentes no cambian (**no breaking**).
- Descubrimiento del catálogo free de OpenRouter (`GET {base_url}/models`, público, sin auth), cacheado con TTL y refresco en background.
- Clasificación por perfil: `architect` prioriza familias de razonamiento y cae a general grande; `code` prioriza familias coder; `writer` prioriza instruct general de contexto largo.
- Expansión en request de cada `openrouter_free` a N backends concretos en su posición, con rotación round-robin; reutiliza el `route_with_failover` existente.
- Blacklist dinámica **global por modelo** ante 429/5xx/`Retry-After`, con `blacklist_cooldown_ms` configurable.
- Fallback a backends de pago declarados en la propia ruta; `AllBackendsFailed` si se agota todo.
- Métricas `llm_gateway_discovery_*` en OTLP/Prometheus.

## Capabilities

### New Capabilities

(ninguna)

### Modified Capabilities

- `gateway`: se amplía la configuración de backends (nuevo `kind`/`profile`/`discovery`), se añade el descubrimiento y clasificación del catálogo free, la expansión/rotación de backends dinámicos, la blacklist dinámica y las métricas asociadas.

## Impact

- **Código**: `src/config.rs` (`Backend`, `Discovery`), `src/proxy.rs` (`route_with_failover` ~L284, `is_retryable` ~L517, `list_models` ~L797), nuevos `src/discovery.rs` y `src/classify.rs`, `src/metrics.rs`.
- **Config**: campos nuevos opcionales; compatibilidad total con `gateway.yaml` existentes.
- **Dependencias/sistemas**: llamada de red a OpenRouter `/models` (público); `OPENROUTER_API_KEY` para inferencia.
- **Documentación**: `docs/openrouter-free-routing.md`.
