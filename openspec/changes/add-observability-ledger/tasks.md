# Tasks

## 1. Parseo de `usage`

- [x] 1.1 Extraer `usage.prompt_tokens`/`usage.completion_tokens` del body en las respuestas no-stream de `src/proxy.rs`, con tests unitarios de respuesta con y sin `usage`
- [x] 1.2 Inyectar `stream_options.include_usage: true` en las peticiones con `stream: true` y leer el `usage` del chunk final, con un test que verifica la inyección y otro que lee el último chunk
- [x] 1.3 Definir la representación de tokens desconocidos (nulos) y verificar con un test que no se registra `0` cuando falta `usage`

## 2. Evento de catálogo (`gateway_catalog`)

- [x] 2.1 Mantener en `src/discovery.rs` el último snapshot de precios por `model_id` y detectar cambios (`new`, `price_up`, `price_down`, `removed`), con tests unitarios de cada transición
- [x] 2.2 Emitir el evento estructurado (campos `ts`, `model_id`, `org`, `pin`, `pout`, `blend`, `context_length`, `change_kind`, `prev_pin`, `prev_pout`) con dedup en memoria, y verificar con tests que un precio sin cambios no emite y que un cambio sí
- [x] 2.3 Verificar con un test que `blend` se calcula como `0.75·pin + 0.25·pout`

## 3. Evento de selección (`gateway_selection`)

- [x] 3.1 Emitir el evento al cambiar el modelo elegido de un `(route, backend)`, con `reason` en `initial`/`cheaper`/`failover`/`removed`, `pin`, `pout` y `prev_model_id`, verificado con tests de las cuatro transiciones (consume el estado de selección de #37)
- [x] 3.2 Verificar con un test que no se emite evento cuando el modelo elegido no cambia

## 4. Evento de petición (`gateway_requests`)

- [x] 4.1 Emitir un evento por petición con `ts`, `request_id`, `route`, `model_id`, `streaming`, `attempts`, `failover`, `outcome`, `finish_reason`, `latency_ms`, `tokens_in`, `tokens_out`, `pin`, `pout`, `cost_usd`, con tests de éxito y de respuesta sin `usage`
- [x] 4.2 Calcular `cost_usd` con los precios del modelo en el momento de la petición, con un test que compara el valor esperado
- [x] 4.3 Verificar con un test de integración que un fallo del exportador no altera la respuesta al cliente

## 5. Métricas

- [ ] 5.1 Añadir `llm_gateway_tokens_total{route,model,type}`, `llm_gateway_cost_usd_total{route,model}` y `llm_gateway_model_selection_total{route,reason}` en `src/metrics.rs`, con tests de incremento
- [ ] 5.2 Añadir `llm_gateway_selected_model_price_usd{route,model,side}` y `llm_gateway_allowlist_price_usd{model}`, con tests de que reflejan el precio actual y de que no incluyen `request_id`

## 6. Configuración, documentación y verificación

- [ ] 6.1 Añadir el bloque `observability` con `events: bool` (default `true`) en `src/config.rs`, con tests de parseo y de default
- [ ] 6.2 Verificar con un test de integración que `observability.events: false` no emite ninguno de los tres eventos
- [ ] 6.3 Crear `docs/observability-ledger.md` con los tres eventos, sus campos, la retención recomendada por evento (configurada en OpenObserve), el requisito de `usage` y consultas SQL de ejemplo (coste por modelo y día; evolución de precio de una familia)
- [ ] 6.4 Verificación manual E2E: `curl -s http://127.0.0.1:4010/metrics | rg 'tokens_total|cost_usd'` y una consulta de coste por modelo y día en OpenObserve
