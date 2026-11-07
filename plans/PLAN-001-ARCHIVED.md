---
# PLAN-001 — Prewarm + keep-alive de backends

## Objetivo

Evitar el arranque en frío (modelo no residente en VRAM) manteniendo calientes los backends OpenAI-compatible mediante un warmup al arrancar y un heartbeat periódico, con observabilidad (gauge warm/cold). Deshabilitado por defecto: sin cambio de comportamiento salvo que el YAML lo pida.

## No-objetivos

- Cache de respuestas (memoización exacta): feature aparte.
- KV/prefix caching: vive en el backend de inferencia, no en el gateway.
- Control de `keep_alive` específico de Ollama (rompería la neutralidad OpenAI-compat).
- Shutdown graceful de las tareas de keep-alive (se detienen con el proceso).

## Contexto / estado actual

- Proxy axum/tokio/reqwest. Ficheros: `src/proxy.rs` (AppState, router, chat_completions, route_with_failover, helpers privados `chat_url`, `resolve_api_key`, `body_for_backend`, `backend_id`), `src/config.rs` (Backend con `base_url`/`model`/`api_key_env`/`timeout_ms`), `src/metrics.rs` (registro atómico pre-registrado), `src/main.rs`.
- Spec vigente: `openspec/specs/gateway/spec.md`.
- Tests: `tests/*.rs` con mocks axum en puerto efímero + `tests/common/mod.rs::dead_backend`.
- CLI `openspec` 1.13.2 disponible.

## Diseño

### Config (`src/config.rs`)

- `Backend` gana `pub warmup: Option<Warmup>` (opcional; ausente = deshabilitado).
- Nuevo `pub struct Warmup { pub interval_ms: u64 } `.
- Validación: si `warmup` está presente e `interval_ms == 0` → error de config nombrando ruta y backend.
- Ejemplo YAML:
```yaml
route:
  - name: mech-writer
    backend:
      - base_url: "http://localhost:11434/v1"
        model: "qwen2.5:7b-instruct"
        warmup:
          interval_ms: 240000
```

### Módulo nuevo `src/warmup.rs`

```rust
pub const WARMUP_TIMEOUT: Duration = Duration::from_secs(30);
fn warmup_body(model: &str) -> Value; // cuerpo mínimo OpenAI-compatible
pub async fn warmup_once(state: &AppState, route: usize, backend: usize) -> bool;
pub fn spawn_warmup(state: AppState) -> Vec<tokio::task::JoinHandle<()>>;
```

- `warmup_once`: construye un cuerpo mínimo (`messages:[{"role":"user","content":"ping"}]`, `max_tokens:1`, sin `stream`), sustituye `model` real vía `body_for_backend`, resuelve la api key, hace POST a `chat_url` con `WARMUP_TIMEOUT`; devuelve `true` si 2xx; registra `metrics.record_warmup(route, backend, ok)` y log (`debug` en éxito, `warn` en fallo).
- `spawn_warmup`: por cada backend con `warmup` presente, `tokio::spawn` un bucle con `tokio::time::interval(Duration::from_millis(interval_ms))` (el primer tick es inmediato → warmup de arranque) que llama a `warmup_once` y nunca aborta por un fallo.
- Helpers de `proxy.rs` pasan a `pub(crate)`: `chat_url`, `resolve_api_key`, `body_for_backend `.
- `src/lib.rs`: añadir `pub mod warmup; `.

### Métricas (`src/metrics.rs`)

- `BackendMetrics` gana: `warmup_enabled: bool`, `warm: AtomicU64` (0/1), `warmups: [AtomicU64; 2]`.
- `Metrics::new`: `warmup_enabled = backend.warmup.is_some() `.
- Nuevo `Metrics::record_warmup(&self, route: usize, backend: usize, ok: bool) `.
- `render()`: solo para backends con `warmup_enabled`, emite el gauge `llm_gateway_backend_warm{route,backend}` y el counter `llm_gateway_warmup_total{route,backend,result}` (`result` en `success`/`failure`).

| Métrica | Tipo | Etiquetas |
|---|---|---|
| `llm_gateway_backend_warm` | gauge | `route`, `backend` |
| `llm_gateway_warmup_total` | counter | `route`, `backend`, `result` |

### Arranque (`src/main.rs`)

- Construir `AppState`, luego `router(state.clone())` y `warmup::spawn_warmup(state)` antes de `axum::serve`.

## Tareas TDD

### RED — tests que fallan
1. `tests/config.rs`: `warmup` ausente → `None`; presente → parsea `interval_ms`; `interval_ms: 0` → error de validación.
2. `tests/warmup.rs`: mock con contador de peticiones; sin `warmup` → 0 peticiones de warmup.
3. `tests/warmup.rs`: `warmup_once` OK → el backend recibe `POST /v1/chat/completions` con el `model` real, `max_tokens=1`, sin `stream`; devuelve `true`; `backend_warm`=1 y `warmup_total{result="success"}`=1.
4. `tests/warmup.rs`: `warmup_once` contra `dead_backend()` → `false`; `backend_warm`=0; `warmup_total{result="failure"}`=1.
5. `tests/warmup.rs`: `spawn_warmup` con intervalo corto (p. ej. 50 ms) → ≥2 warmups en ~200 ms.
6. `tests/warmup.rs`: `api_key_env` se respeta (el mock valida `Authorization: Bearer <valor>`).
7. Ejecutar `cargo test`: los nuevos fallan y el resto sigue verde.

### GREEN — implementación mínima
1. `config.rs`: `Warmup` + campo + validación.
2. `warmup.rs`: constantes, `warmup_body`, `warmup_once`, `spawn_warmup `.
3. `proxy.rs`: helpers a `pub(crate) `.
4. `lib.rs`: `pub mod warmup; `.
5. `metrics.rs`: `warmup_enabled`/`warm`/`warmups` + `record_warmup` + `render `.
6. `main.rs`: `spawn_warmup `.
7. `cargo test` en verde y `cargo check` sin errores.

### REFACTOR
- `cargo fmt --all -- --check`.
- `cargo clippy --all-targets -- -D warnings`.
- `cargo test` ×5 sin fallos.

### Docs y verificación
- `README.md`: sección keep-alive (YAML `warmup`, métricas nuevas).
- `gateway.yaml.example`: ejemplo de `warmup`.
- `docs/backend-warmup.md`: qué es, ficheros implicados, cómo funciona, config, cómo habilitar.
- Verificación manual: backend Ollama con `warmup.interval_ms` y `curl /metrics | grep backend_warm `.

## Estado

- [x] RED
- [x] GREEN
- [x] REFACTOR
- [x] Docs
- [ ] Verificación manual
---