# Tasks

## 1. Gate de cuota (reactivo)

- [ ] 1.1 Añadir al `Registry`/`Discovery` el estado global `free_quota_exhausted_until` con métodos `open_quota_gate(deadline)` y `quota_gate_open()`, y verificarlo con tests unitarios de apertura y expiración (`cargo test`)
- [ ] 1.2 Implementar en `src/proxy.rs` la detección del `429` de cuota diaria (`error.metadata.limit_source == openrouter_free_tier_daily` y heurística `X-RateLimit-Remaining: 0` + `X-RateLimit-Reset` > 5 min), y verificar con tests que el `429` diario abre el gate y el de minuto/proveedor no
- [ ] 1.3 Calcular T desde `X-RateLimit-Reset` (epoch en ms) con fallback al siguiente `00:00 UTC` y tope `MAX_QUOTA_GATE` (24 h), verificado con tests de los tres casos
- [ ] 1.4 Hacer que la expansión dinámica devuelva cero candidatos con el gate abierto y verificar con un test que la ruta salta al backend de pago

## 2. Sondeo proactivo

- [ ] 2.1 Implementar el parseo de `GET {base_url}/key` a `free_model_daily_requests.remaining` y verificarlo con tests sobre un servidor HTTP simulado
- [ ] 2.2 Añadir la tarea de sondeo en background (patrón `spawn_refresh`), una por `base_url`, con `quota_poll_interval_ms` y `stale-while-error`, verificada con un test de cadencia y otro de fallo que conserva el estado
- [ ] 2.3 Abrir el gate desde el sondeo cuando `remaining == 0` (deadline siguiente `00:00 UTC`) y verificar con un test de integración del ciclo de sondeo

## 3. Configuración

- [ ] 3.1 Añadir `respect_daily_quota` (default `true`) y `quota_poll_interval_ms` (default `60000`) al bloque `discovery`, con validación (`quota_poll_interval_ms == 0` es error) y tests de parseo, defaults y validación
- [ ] 3.2 Verificar que `respect_daily_quota: false` desactiva gate y sondeo con un test de integración

## 4. Métricas

- [ ] 4.1 Añadir el gauge `llm_gateway_free_quota_remaining{base_url}` y el counter `llm_gateway_free_quota_skips_total{route}` en `src/metrics.rs`, con tests de emisión y de ausencia del gauge sin observación

## 5. Respaldo de pago dinámico

- [x] 5.1 Añadir la variante `BackendKind::OpenrouterPaid` (serde `openrouter_paid`) en `src/config.rs`, reutilizando `base_url`, `api_key`, `profile` y `min_context`, con validación (base_url y api_key presentes, profile válido) y tests de parseo y de campo ausente
- [x] 5.2 Añadir en `Discovery` los campos `allow` (Vec<String>), `reselect_interval_ms` (default 14400000) y `min_price_delta` (default 0.20), con defaults, validación (`min_price_delta >= 0`, ningún patrón vacío) y tests de parseo, defaults y validación
- [x] 5.3 Implementar la elegibilidad de pago (allow + tool-calling + `pricing > 0` + `min_context` + excluir `:batch`) y el orden por `blend` ascendente en `src/classify.rs` (modo `Price` en `ClassifyContext` o `classify_paid` + helper `matches_allow`), con tests unitarios de filtrado, exclusión del free, orden y desempates (contexto desc, id asc)
- [x] 5.4 Hacer que la expansión resuelva un backend `openrouter_paid` reutilizando el catálogo cacheado y **sin rotación**, con failover en orden de precio, verificado con un test de que dos expansiones consecutivas devuelven el mismo primer candidato
- [x] 5.5 Implementar la selección pegajosa con histéresis en el `Registry` (estado por `(route, backend)`: modelo, instante de elección y precio), con tests de: mantener dentro de `reselect_interval_ms`, cambiar tras el intervalo con mejora ≥ `min_price_delta`, no cambiar con mejora marginal, cambio inmediato por fallo o baja del catálogo, y elección inicial tras reinicio
- [x] 5.6 Emitir un aviso (log) cuando un patrón de `allow` no tiene coincidencias en el catálogo, con un test que lo verifica sin elevar el nivel a error
- [x] 5.7 Verificar con un test que `allow` ausente o vacío hace que el backend de pago aporte cero candidatos y la ruta continúe con el siguiente backend

## 6. Ejemplo, documentación y verificación de integración

- [x] 6.1 Actualizar `gateway.yaml.example`: respaldo `kind: openrouter_paid` con `profile` en `architect`/`code`/`writer`, `discovery.allow` con la lista por defecto y un `kind: static` final; verificar con los tests de configuración que el ejemplo sigue cargando (`cargo test`)
- [x] 6.2 Documentar el gate, el sondeo, las métricas, `openrouter_paid`, la lista curada, la cadencia de selección y la histéresis en `docs/openrouter-free-routing.md`, y anotar los follow-ups en `docs/openrouter-free-followups.md`
- [x] 6.3 Test de integración: con cuota agotada simulada, la ruta `code` responde desde el backend de pago sin ningún intento free, el contador de saltos incrementa y el modelo servido es el más barato de la lista curada
- [ ] 6.4 Verificación manual E2E: `curl -s http://127.0.0.1:4010/metrics | rg free_quota` y una inferencia real con `finish_reason: stop`