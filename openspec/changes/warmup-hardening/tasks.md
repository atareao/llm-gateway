# Tasks

## 1. RED — tests que fallan

- [ ] 1.1 `tests/config.rs`: `warmup.timeout_ms` ausente → `None`; presente → parsea el valor; `timeout_ms: 0` → error de validación que nombra ruta y backend.
- [ ] 1.2 `tests/warmup.rs`: `spawn_warmup` con `interval_ms = 50` y un mock que tarda > 50 ms → tras la recuperación NO se observa ráfaga (nº de peticiones acotado, p. ej. ≤ 3 en ~300 ms).
- [ ] 1.3 `tests/warmup.rs`: backend con `timeout_ms` mayor que el retardo del mock → el warmup registra `success`; sin `timeout_ms` y con retardo > default → `failure`.
- [ ] 1.4 `tests/warmup.rs`: `api_key_env` sin resolver → exactamente una `warmup_total{result="failure"}` y cero peticiones HTTP tras varios ticks; el `warn` no se repite (aserción por comportamiento).
- [ ] 1.5 `tests/warmup.rs`: backend caído (error de transporte) → el warmup registra `failure` y la ruta de error incluye la causa (verificable por helper o por ausencia de descarte; ajustar a lo observable).
- [ ] 1.6 Ejecutar `cargo test` y confirmar que 1.1–1.5 fallan y el resto sigue verde.

## 2. GREEN — implementación mínima

- [ ] 2.1 `src/config.rs`: `Warmup.timeout_ms: Option<u64>` + validación `timeout_ms > 0` cuando está presente.
- [ ] 2.2 `src/warmup.rs`: `ticker.set_missed_tick_behavior(MissedTickBehavior::Delay)` en `spawn_warmup`.
- [ ] 2.3 `src/warmup.rs`: usar `warmup.timeout_ms` (o `WARMUP_TIMEOUT` si `None`) como timeout de la petición.
- [ ] 2.4 `src/warmup.rs`/`src/proxy.rs`: resolver la API key una vez por tarea; `warn` y `failure` como máximo una vez si no resuelve; ticks posteriores en `debug` sin HTTP.
- [ ] 2.5 `src/warmup.rs`: registrar la causa del `reqwest::Error` en el `warn` de fallo de transporte.
- [ ] 2.6 `src/warmup.rs`: guardia `if interval.is_zero() { continue; }`.
- [ ] 2.7 `src/main.rs`: conservar los `JoinHandle` y abortarlos en cierre ordenado (SIGINT/SIGTERM) con `with_graceful_shutdown`.
- [ ] 2.8 `cargo test` en verde y `cargo check` sin errores.

## 3. REFACTOR

- [ ] 3.1 `cargo fmt --all -- --check`.
- [ ] 3.2 `cargo clippy --all-targets -- -D warnings`.
- [ ] 3.3 `cargo test` de nuevo, 5 ejecuciones sin fallos.

## 4. Docs

- [ ] 4.1 `gateway.yaml.example`: añadir `warmup.timeout_ms` comentado.
- [ ] 4.2 `docs/backend-warmup.md`: documentar `timeout_ms`, `MissedTickBehavior::Delay`, aviso único de clave y apagado ordenado; actualizar límites/follow-ups.
- [ ] 4.3 `README.md`: reflejar `timeout_ms` en el ejemplo de keep-alive.
- [ ] 4.4 Doc-comments actualizados en `src/warmup.rs` y `src/config.rs`.

## 5. Verificación manual

- [ ] 5.1 Ollama real con `qwen3:14b` y `timeout_ms` acorde al cold load (~90 s): el warmup de arranque registra `success` (sin los 2 fallos por timeout de la v1) y no se observa ráfaga de ticks.
