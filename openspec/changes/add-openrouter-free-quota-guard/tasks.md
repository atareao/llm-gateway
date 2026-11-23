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

## 5. Respaldo de pago y documentación

- [ ] 5.1 Cambiar el modelo de respaldo de pago a `z-ai/glm-5.3-flash` en `gateway.yaml.example` y verificar con los tests de configuración que el ejemplo sigue cargando (`cargo test`)
- [ ] 5.2 Documentar el gate, el sondeo, las métricas y el flag en `docs/openrouter-free-routing.md`, y anotar los follow-ups en `docs/openrouter-free-followups.md`

## 6. Verificación de integración

- [ ] 6.1 Test de integración: con cuota agotada simulada, la ruta `code` responde desde el backend de pago sin ningún intento free y el contador de saltos incrementa
- [ ] 6.2 Verificación manual E2E: `curl -s http://127.0.0.1:4010/metrics | rg free_quota` y una inferencia real con `finish_reason: stop`
