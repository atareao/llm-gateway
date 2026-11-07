# Tasks

## 1. RED — tests que fallan

- [x] 1.1 `tests/config.rs`: `warmup.timeout_ms` ausente → `None`; presente → parsea el valor; `timeout_ms: 0` → error de validación que nombra ruta y backend.
- [x] 1.2 `tests/warmup.rs`: `spawn_warmup` con `interval_ms = 50` y un mock que tarda > 50 ms → tras la recuperación NO se observa ráfaga (nº de peticiones acotado, p. ej. ≤ 3 en ~300 ms).
- [x] 1.3 `tests/warmup.rs`: backend con `timeout_ms` mayor que el retardo del mock → el warmup registra `success`; sin `timeout_ms` y con retardo > default → `failure`.
- [x] 1.4 `tests/warmup.rs`: `api_key_env` sin resolver → exactamente una `warmup_total{result="failure"}` y cero peticiones HTTP tras varios ticks; el `warn` no se repite (aserción por comportamiento).
- [x] 1.5 `tests/warmup.rs`: backend caído (error de transporte) → el warmup registra `failure` y la ruta de error incluye la causa (verificable por helper o por ausencia de descarte; ajustar a lo observable).
- [x] 1.6 Ejecutar `cargo test` y confirmar que 1.1–1.5 fallan y el resto sigue verde.

## 2. GREEN — implementación mínima

- [x] 2.1 `src/config.rs`: `Warmup.timeout_ms: Option<u64>` + validación `timeout_ms > 0` cuando está presente.
- [x] 2.2 `src/warmup.rs`: `ticker.set_missed_tick_behavior(MissedTickBehavior::Delay)` en `spawn_warmup`.
- [x] 2.3 `src/warmup.rs`: usar `warmup.timeout_ms` (o `WARMUP_TIMEOUT` si `None`) como timeout de la petición.
- [x] 2.4 `src/warmup.rs`/`src/proxy.rs`: resolver la API key una vez por tarea; `warn` y `failure` como máximo una vez si no resuelve; ticks posteriores en `debug` sin HTTP.
- [x] 2.5 `src/warmup.rs`: registrar la causa del `reqwest::Error` en el `warn` de fallo de transporte.
- [x] 2.6 `src/warmup.rs`: guardia `if interval.is_zero() { continue; }`.
- [x] 2.7 `src/main.rs`: conservar los `JoinHandle` y abortarlos en cierre ordenado (SIGINT/SIGTERM) con `with_graceful_shutdown`.
- [x] 2.8 `cargo test` en verde y `cargo check` sin errores.

## 3. REFACTOR

- [x] 3.1 `cargo fmt --all -- --check`.
- [x] 3.2 `cargo clippy --all-targets -- -D warnings`.
- [x] 3.3 `cargo test` de nuevo, 5 ejecuciones sin fallos.

## 4. Docs

- [x] 4.1 `gateway.yaml.example`: añadir `warmup.timeout_ms` comentado.
- [x] 4.2 `docs/backend-warmup.md`: documentar `timeout_ms`, `MissedTickBehavior::Delay`, aviso único de clave y apagado ordenado; actualizar límites/follow-ups.
- [x] 4.3 `README.md`: reflejar `timeout_ms` en el ejemplo de keep-alive.
- [x] 4.4 Doc-comments actualizados en `src/warmup.rs` y `src/config.rs`.

## 5. Verificación manual

- [x] 5.1 Verificado con Ollama real (2026-11-07, `qwen3:14b`, `interval_ms: 5000`, `timeout_ms: 200000`) forzando cold start (`ollama stop`): `warmup_total{result="success"}=8 / failure=0` pese a reloads en frío de ~70 s (>30 s) → el timeout configurable funciona. Sin ráfaga tras el bloqueo (siguiente warmup a `interval_ms`, no 14 ticks). Cierre ordenado: `SIGTERM` → log `cierre ordenado completado tasks=1` y puerto liberado. con `qwen3:14b` y `timeout_ms` acorde al cold load (~90 s): el warmup de arranque registra `success` (sin los 2 fallos por timeout de la v1) y no se observa ráfaga de ticks.
