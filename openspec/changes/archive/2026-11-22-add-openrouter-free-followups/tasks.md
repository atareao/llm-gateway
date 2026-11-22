# Tasks

## 0. Preparación (baseline)

- [x] 0.1 Ejecutar `cargo test`, `cargo clippy --all-targets --all-features -- -D warnings` y `cargo fmt --check` en la rama `chore/plan-008-followups` para fijar la línea base en verde

## 1. Configuración: `Discovery` con defaults (N1)

- [x] 1.1 TEST (RED): añadir tests en `src/config.rs` para `discovery: { min_context: 32768 }`, `discovery: {}` y `min_context` junto a un TTL explícito; verificar que hoy fallan por campos faltantes (`cargo test`)
- [x] 1.2 GREEN: añadir `#[serde(default = "default_refresh_interval_ms")]` y `#[serde(default = "default_blacklist_cooldown_ms")]` a `Discovery` con las constantes ya existentes y confirmar los tests en verde
- [x] 1.3 REFACTOR: mantener la validación de `0` explícito como error, con test que comprueba ruta, backend y campo en el mensaje

## 2. Configuración: TTL coherente por `base_url` (M6)

- [x] 2.1 TEST (RED): test que carga una config con dos backends dinámicos de la misma `base_url` y TTLs distintos y espera error con la `base_url` y las rutas (`cargo test`)
- [x] 2.2 GREEN: implementar la validación cruzada en `GatewayConfig::validate` (o el punto de validación existente) recorriendo los backends dinámicos agrupados por `base_url`
- [x] 2.3 TEST: caso positivo con TTLs iguales y caso con un backend sin `discovery` más otro con TTL explícito (compatible con el default)
- [x] 2.4 REFACTOR: extraer el mensaje de error a una función con formato único para el resto de validaciones

## 3. Clasificación: aviso de `include` no elegible (N2)

- [x] 3.1 TEST (RED): test en `src/classify.rs` que pasa un `include` ausente y comprueba que la lista no lo contiene (hoy pasa en verde) y que existe la señal de aviso vía el punto de observabilidad elegido
- [x] 3.2 GREEN: emitir `tracing::warn!` con `model`, `profile` y `base_url`, pasando la `base_url` a `classify` (o a su contexto) sin cambiar la salida
- [x] 3.3 TEST: `include` presente y elegible no genera warning; `include` filtrado por `min_context` sí lo genera
- [x] 3.4 REFACTOR: ajustar firmas y call sites (`discovery.rs`) sin duplicar parámetros

## 4. Rendimiento: candidato ligero (M5)

- [x] 4.1 TEST (RED): tests en `src/proxy.rs` que fijan el contrato observable de la expansión (orden/posición en la ruta, `model` expandido, `base_url`, `timeout_ms` efectivo y `api_key_env` arrastrados del backend, y `dynamic_cooldown` solo en dinámicos) con una ruta que combina un estático y un dinámico con `overrides` y `warmup`
- [x] 4.2 GREEN: sustituir `Candidate.backend: Backend` por campos ligeros (`slot`, `model`, `base_url`, `timeout_ms`, `api_key_env`, `dynamic_cooldown`) y adaptar `route_with_failover`, `blacklist_if_needed` y la construcción de la petición
- [x] 4.3 TEST: verificar que el failover, el timeout por defecto y el marcado en blacklist siguen produciendo los mismos valores (tests existentes de failover en verde)
- [x] 4.4 REFACTOR: eliminar clones innecesarios de `String` y revisar que `expand_backends` no clona `Backend`

## 5. Rendimiento: deduplicación e índice (N3)

- [x] 5.1 TEST (RED): test de `eligible_model_ids` con dos perfiles solapados que espera identificadores únicos y en orden estable
- [x] 5.2 GREEN: deduplicar con `HashSet` preservando el orden de primera aparición
- [x] 5.3 GREEN: indexar `DynamicBackend` por `(route, backend)` (HashMap o `Vec<Vec<..>>`) en `Registry::new` y eliminar la búsqueda lineal de `dynamic()`
- [x] 5.4 TEST: config con dos rutas cuyos backends dinámicos ocupan índices homónimos (y cruzados) con distinto significado; cada `(ruta, índice)` resuelve su propio cooldown y los slots estáticos devuelven `None`

## 6. Métricas: escritura del gauge solo ante cambios

- [x] 6.1 TEST (RED): test que comprueba que accesos sucesivos sin cambio de tamaño conservan el valor del gauge
- [x] 6.2 GREEN: guardar el último tamaño publicado en `Metrics` (`AtomicUsize`, inicializado a 0) y escribir el gauge solo si difiere; `Blacklist::purge_and_report` conserva firma y comportamiento
- [x] 6.3 TEST: marca, expiración y nueva marca actualizan el gauge correctamente

## 7. Tests: carrera de entorno (N4)

- [x] 7.1 TEST: reproducir el patrón actual con `--test-threads` por defecto y confirmar la fuente de la carrera en `tests/dynamic_routing.rs`
- [x] 7.2 GREEN: fijar las variables de entorno una vez por proceso (helper con `OnceLock`) o serializar los tests afectados; sin cambios en las aserciones
- [x] 7.3 Ejecutar el binario de tests repetidamente (`for i in $(seq 20); do cargo test --test dynamic_routing || break; done`) sin fallos

## 8. Cobertura de escenarios

- [x] 8.1 Test: catálogo vacío `{"data": []}` → cero candidatos, sin error
- [x] 8.2 Test: TTL vencido combinado con fallo de red → se usa caché previa o cero candidatos, sin pánico
- [x] 8.3 Test: blacklist compartida entre dos rutas con la misma `base_url` → el modelo marcado desde una no se propone en la otra

## 9. Documentación

- [x] 9.1 Crear `docs/openrouter-free-followups.md` (qué es, ficheros, funcionamiento, config, decisiones, follow-ups pendientes)
- [x] 9.2 Actualizar `docs/openrouter-free-routing.md` en la sección de `discovery`: defaults por campo y TTL único por `base_url`
- [x] 9.3 Verificar que `gateway.yaml.example` sigue validando con `cargo run -- --check-config` (o el comando equivalente documentado). No existe `--check-config`: se verificó con `GATEWAY_CONFIG=gateway.yaml.example GATEWAY_LISTEN=127.0.0.1:0 timeout 8 cargo run --quiet`, que registró `configuración cargada routes=5` antes de escuchar.

## 10. Verificación final

- [x] 10.1 `cargo test` completo en verde
- [x] 10.2 `cargo clippy --all-targets --all-features -- -D warnings` sin warnings
- [x] 10.3 `cargo fmt --check` limpio
- [x] 10.4 `openspec validate add-openrouter-free-followups --strict`
- [x] 10.5 Comprobar cada escenario del delta implementado y cubierto por un test; marcar tareas