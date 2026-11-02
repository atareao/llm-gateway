# Tasks

## 1. RED — tests que fallan

- [x] 1.1 `tests/metrics.rs`: `GET /metrics` responde `200`, `text/plain; version=0.0.4` y contiene `# TYPE llm_gateway_requests_total counter`.
- [x] 1.2 `tests/metrics.rs`: tras una petición con éxito, `llm_gateway_requests_total{route="mech-writer",streaming="false",outcome="success"} 1` y `llm_gateway_failover_total{route="mech-writer"} 0`.
- [x] 1.3 `tests/metrics.rs`: con primario caído, `llm_gateway_failover_total{route="mech-writer"} 1`, primario con `result="transport_error"` y fallback con `result="success"`.
- [x] 1.4 `tests/metrics.rs`: primario `503` → `result="retryable_status"` y respuesta `200`.
- [x] 1.5 `tests/metrics.rs`: todos caídos → `502` y `outcome="all_failed"`.
- [x] 1.6 `tests/metrics.rs`: `model` desconocido → `404` y `llm_gateway_errors_total{kind="model_not_found"} 1`.
- [x] 1.7 `tests/metrics.rs`: `x-request-id` se reutiliza si es válido, se genera con prefijo `req-` si falta y se descarta si mide más de 128 bytes.
- [x] 1.8 `src/metrics.rs` (unitarios): histograma acumula buckets correctamente, `+Inf` igual al total y las etiquetas se escapan.
- [x] 1.9 Ejecutar `cargo test` y confirmar que 1.1–1.8 fallan y el resto sigue verde.

## 2. GREEN — implementación mínima

- [x] 2.1 `src/metrics.rs`: `Metrics`, `RouteMetrics`, `BackendMetrics`, `Histogram`, enums `Outcome`/`GlobalOutcome`/`AttemptResult` y `render()`.
- [x] 2.2 `src/lib.rs`: exportar `pub mod metrics;`.
- [x] 2.3 `AppState`: añadir `metrics: Arc<Metrics>` construido en `new()`.
- [x] 2.4 `router()`: añadir `GET /metrics`.
- [x] 2.5 `chat_completions`: resolver índice de ruta, generar/reutilizar `x-request-id`, abrir span, medir latencia y registrar `outcome`/`errors`.
- [x] 2.6 `route_with_failover`: recibir índices, medir cada intento y registrar `AttemptResult` + `failover_total`.
- [x] 2.7 Añadir `x-request-id` a la respuesta (éxito y error).
- [x] 2.8 `main.rs`: `GATEWAY_LOG_FORMAT=json` → formateador JSON; activar feature `json` en `Cargo.toml`.
- [x] 2.9 `cargo test` en verde y `cargo check` sin errores.

## 3. REFACTOR

- [x] 3.1 `cargo fmt --all -- --check`.
- [x] 3.2 `cargo clippy --all-targets -- -D warnings`.
- [x] 3.3 `cargo test` de nuevo, 5 ejecuciones sin fallos.

## 4. Docs

- [x] 4.1 `README.md`: sección de observabilidad (`/metrics`, tabla de métricas, `x-request-id`, `GATEWAY_LOG_FORMAT`).
- [x] 4.2 Doc-comments en `src/metrics.rs` y en los puntos de instrumentación de `src/proxy.rs`.

## 5. Verificación manual

- [x] 5.1 Arrancar el gateway con un backend caído y comprobar `curl localhost:8080/metrics` tras una petición.
- [x] 5.2 Arrancar con `GATEWAY_LOG_FORMAT=json` y comprobar que cada línea es JSON válido.

## 6. Git Flow

- [x] 6.1 Rama `feature/add-observability` desde `development`.
- [x] 6.2 Commits con gitmoji + PR a `development`.
- [ ] 6.3 `openspec archive add-observability` tras el merge.
