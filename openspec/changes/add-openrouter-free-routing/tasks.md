# Tasks

## 1. Configuración y tipos

- [x] 1.1 Añadir `BackendKind`, `Profile` y `Discovery` a `src/config.rs` con defaults compatibles (`kind` default `static`) y verificar que un `gateway.yaml` existente sin `kind` carga igual (`cargo test`)
- [x] 1.2 Implementar validación semántica (static sin `model`, openrouter_free sin `api_key_env`, perfil inválido) y verificar con tests unitarios que cada caso devuelve error con ruta y backend identificados

## 2. Descubrimiento

- [x] 2.1 Implementar `src/discovery.rs` (consulta `GET /models`, filtro free + tool-calling, caché con TTL) y verificar con tests que usan un servidor HTTP simulado
- [x] 2.2 Añadir refresco en segundo plano con `stale-while-error` y verificar con un test que un fallo de red conserva la caché válida
- [x] 2.3 Integrar el arranque del refresco siguiendo el patrón de `src/warmup.rs` y verificar el arranque local (`cargo run` con config de prueba)

## 3. Clasificación

- [x] 3.1 Implementar `src/classify.rs` con los tres perfiles y orden por `context_length` desc y `id` asc, verificado con tests por familia (razonamiento, coder, general)
- [x] 3.2 Añadir overrides manuales por configuración y verificar con un test que un modelo excluido no aparece en el perfil
- [x] 3.3 Aplicar `discovery.min_context` y verificar con un test que los modelos con contexto insuficiente se descartan

## 4. Integración en el enrutado

- [x] 4.1 Expandir backends dinámicos en su posición con rotación round-robin dentro de `route_with_failover` y verificar con tests de expansión y rotación entre peticiones
- [x] 4.2 Implementar la blacklist global con cooldown y `Retry-After` y verificar con tests para `429`, `5xx`, `Retry-After` y expiración del cooldown
- [x] 4.3 Asegurar el fallback a backends de pago y `AllBackendsFailed` al agotar la ruta, verificado con tests de agotamiento
- [x] 4.4 Reflejar los modelos dinámicos elegibles en `list_models` (`src/proxy.rs`) y verificar con un test del endpoint

## 5. Métricas

- [x] 5.1 Registrar las métricas `llm_gateway_discovery_*` en `src/metrics.rs` y verificar con un test de registro e incremento

## 6. Documentación

- [x] 6.1 Escribir `docs/openrouter-free-routing.md` (qué es, ficheros, funcionamiento, variables/config, despliegue, decisiones, follow-ups) y verificar que los comandos del documento se ejecutan tal cual
- [x] 6.2 Añadir a `gateway.yaml.example` las tres rutas de ejemplo y verificar que la config de ejemplo valida

## 7. Verificación de integración

- [ ] 7.1 Ejecutar la suite completa y los linters: `cargo test`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo fmt --check`, sin errores ni warnings
- [ ] 7.2 Prueba end-to-end local con las tres rutas contra OpenRouter y comprobar el failover free→pago y las métricas expuestas
