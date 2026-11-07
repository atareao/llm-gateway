# Tasks

## 1. RED — tests que fallan

- [x] 1.1 `tests/config.rs`: `warmup` ausente → `None`; presente → parsea `interval_ms`; `interval_ms: 0` → error de validación.
- [x] 1.2 `tests/warmup.rs`: mock con contador; sin `warmup` → 0 peticiones de warmup.
- [x] 1.3 `tests/warmup.rs`: `warmup_once` OK → el backend recibe `POST /v1/chat/completions` con `model` real, `max_tokens: 1`, sin `stream`; devuelve `true`; `backend_warm`=1 y `warmup_total{result="success"}`=1.
- [x] 1.4 `tests/warmup.rs`: `warmup_once` contra `dead_backend()` → `false`; `backend_warm`=0; `warmup_total{result="failure"}`=1.
- [x] 1.5 `tests/warmup.rs`: `spawn_warmup` con `interval_ms = 50` → ≥2 warmups en ~200 ms.
- [x] 1.6 `tests/warmup.rs`: `api_key_env` resuelto → el mock recibe `Authorization: Bearer <valor>`; variable ausente → `failure` sin petición HTTP.
- [x] 1.7 Ejecutar `cargo test` y confirmar que 1.1–1.6 fallan y el resto sigue verde.

## 2. GREEN — implementación mínima

- [x] 2.1 `src/config.rs`: `pub struct Warmup { pub interval_ms: u64 }`, campo `pub warmup: Option<Warmup>` en `Backend` y validación `interval_ms > 0`.
- [x] 2.2 `src/warmup.rs`: `WARMUP_TIMEOUT`, `warmup_body`, `warmup_once`, `spawn_warmup`.
- [x] 2.3 `src/proxy.rs`: `chat_url`, `resolve_api_key` y `body_for_backend` a `pub(crate)`.
- [x] 2.4 `src/lib.rs`: `pub mod warmup;`.
- [x] 2.5 `src/metrics.rs`: `warmup_enabled`, `warm`, `warmups` en `BackendMetrics`, `record_warmup` y render de las dos series.
- [x] 2.6 `src/main.rs`: `warmup::spawn_warmup(state)` antes de `axum::serve`.
- [x] 2.7 `cargo test` en verde y `cargo check` sin errores.

## 3. REFACTOR

- [x] 3.1 `cargo fmt --all -- --check`.
- [x] 3.2 `cargo clippy --all-targets -- -D warnings`.
- [x] 3.3 `cargo test` de nuevo, 5 ejecuciones sin fallos.

## 4. Docs

- [x] 4.1 `README.md`: sección keep-alive (YAML `warmup` + tabla de métricas).
- [x] 4.2 `gateway.yaml.example`: ejemplo de `warmup`.
- [x] 4.3 `docs/backend-warmup.md`.
- [x] 4.4 Doc-comments en `src/warmup.rs`.

## 5. Verificación manual

- [x] 5.1 Verificado con Ollama real (2026-11-07, `qwen3:14b`, `interval_ms: 5000`): gauge `llm_gateway_backend_warm{route="ollama-local"}=1`, `warmup_total{result="success"}=19 / failure=2`, modelo residente en `ollama ps` con `expires_at` renovado, y `requests_total` sin contaminar. Nota: el cold load de un 14B (~90s) supera `WARMUP_TIMEOUT` (30s) → los 2 primeros warmups fallan.
