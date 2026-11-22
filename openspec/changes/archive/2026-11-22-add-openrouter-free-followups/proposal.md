# Proposal

## Why

La auditoría de PLAN-008 (rutas dinámicas OpenRouter free, issue #31) dejó follow-ups no bloqueantes (issue #32) que afectan al coste del hot path, a la robustez de la configuración y a la cobertura de escenarios. Ninguno impide el uso actual, pero el clonado del `Backend` completo por cada modelo expandido y en cada petición crece con el catálogo, y varias configuraciones válidas fallan o se ignoran en silencio, lo que dificulta la operación.

## What Changes

- **M5 (rendimiento):** sustituir en `src/proxy.rs` el `Candidate` que transporta un `Backend` completo por un candidato ligero con solo `model`, `base_url`, `timeout_ms` y `api_key_env`, eliminando el clon de `discovery`/`overrides`/`warmup` en el hot path. Sin cambio de comportamiento observable.
- **N3 (rendimiento):** deduplicar `Registry::eligible_model_ids` con un `HashSet` en lugar de `Vec::contains` (O(n²)) y sustituir la búsqueda lineal de `Registry::dynamic` por un índice directo `(route, backend)`. Sin cambio de comportamiento.
- **Gauge (métricas):** `purge_and_report` solo escribe el gauge `llm_gateway_discovery_blacklist` cuando el tamaño cambia, evitando escrituras repetidas en cada acceso. El gauge sigue reflejando expiraciones de cooldown.
- **M6 (configurabilidad):** el TTL del catálogo por `base_url` deja de resolverse en silencio con el primer backend dinámico: se valida en carga que todos los backends dinámicos que comparten `base_url` declaran el mismo `refresh_interval_ms`, y un conflicto es error de configuración con las rutas afectadas. Sin `discovery`, se usa el TTL por defecto ya existente.
- **N1 (configurabilidad):** `Discovery` gana defaults por campo (`refresh_interval_ms` y `blacklist_cooldown_ms`), de modo que `discovery: { min_context: 32768 }` es válido y equivalente a declarar los valores por defecto. Los valores explícitos `0` siguen siendo error de validación.
- **N2 (observabilidad):** un `include` de `discovery.overrides` que nombra un modelo no elegible (filtrado por `min_context`, `exclude` o ausente del catálogo) deja de descartarse en silencio: se emite un `tracing::warn!` con perfil, `base_url` y el identificador ignorado. La lista resultante no cambia.
- **N4 (tests):** eliminar la carrera `set_var`/`getenv` de `tests/dynamic_routing.rs` fijando las variables de entorno una sola vez por proceso o serializando los tests que dependen de ellas, sin relajar ninguna aserción.
- **Cobertura:** añadir escenarios explícitos para catálogo vacío `{"data":[]}`, TTL vencido combinado con fallo de red, y blacklist compartida entre dos rutas con la misma `base_url`.

No hay cambios breaking: la configuración existente y las respuestas HTTP se mantienen idénticas.

## Capabilities

### New Capabilities

(ninguna)

### Modified Capabilities

- `gateway`: defaults de `Discovery`, validación de TTL coherente por `base_url`, aviso ante `include` no elegible, candidato ligero en la expansión de rutas dinámicas, deduplicación en `/v1/models`, escritura del gauge solo ante cambios, y escenarios de prueba adicionales.

## Impact

- **Código:** `src/proxy.rs` (`Candidate`, `expand_backends`, `route_with_failover`), `src/discovery.rs` (`Registry::new`, `dynamic`, `eligible_model_ids`, `Blacklist::purge_and_report`), `src/config.rs` (`Discovery` con `#[serde(default = "...")]` y validación de TTL), `src/classify.rs` (aviso de `include` no elegible), `tests/dynamic_routing.rs`.
- **Config:** `gateway.yaml` existentes siguen siendo válidos; una config con TTLs distintos para la misma `base_url` pasa de "se ignora en silencio" a error explícito.
- **Documentación:** `docs/openrouter-free-followups.md` y actualización de `docs/openrouter-free-routing.md` donde se describa el TTL por `base_url`.
- **Sin cambios** en API pública, métricas expuestas ni dependencias.