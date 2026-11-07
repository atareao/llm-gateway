# Proposal: add-backend-warmup

## Why

Un backend local (p. ej. Ollama) descarga el modelo de VRAM tras unos minutos de inactividad: la siguiente petición paga un arranque en frío de segundos o minutos. El gateway ya aporta failover, pero no evita esa penalización de latencia. Mantener el modelo residente con un warmup de arranque y un heartbeat periódico la elimina.

## What Changes

- `Backend` gana un campo opcional `warmup: { interval_ms: u64 }`. Ausente = deshabilitado (sin cambio de comportamiento).
- Nuevo módulo `src/warmup.rs`: `warmup_once` (una petición mínima OpenAI-compatible) y `spawn_warmup` (una tarea de keep-alive por backend con `warmup`, con warmup inmediato al arrancar y repetición cada `interval_ms`).
- Validación de config: `interval_ms` debe ser > 0.
- Observabilidad: gauge `llm_gateway_backend_warm{route,backend}` (1 = último warmup OK, 0 = frío) y counter `llm_gateway_warmup_total{route,backend,result}`.
- `main.rs` lanza las tareas de keep-alive antes de servir.
- Doc en `README.md`, `gateway.yaml.example` y `docs/backend-warmup.md`.

## Non-goals

- Cache/memoización de respuestas: fuera de este change.
- KV/prefix caching: vive en el backend de inferencia, no en el gateway.
- `keep_alive` específico de Ollama: rompería la neutralidad OpenAI-compat.
- Shutdown graceful de las tareas: se detienen con el proceso.
- Reintento/failover específico del warmup: un warmup fallido solo se registra y se reintenta en el siguiente tick.

## Impact

- `AppState` no cambia de forma; `src/main.rs` añade una llamada a `warmup::spawn_warmup(state)`.
- Helpers de `src/proxy.rs` (`chat_url`, `resolve_api_key`, `body_for_backend`) pasan a `pub(crate)`.
- `src/metrics.rs` gana estado y render de las dos series nuevas (cardinalidad acotada por la config).
- Sin dependencias nuevas.
- `src/lib.rs` añade `pub mod warmup;`.

## Contracts

```rust
// src/config.rs
pub struct Warmup { pub interval_ms: u64 }
pub struct Backend { /* ... */ pub warmup: Option<Warmup> }

// src/warmup.rs
pub const WARMUP_TIMEOUT: Duration; // 30 s
pub async fn warmup_once(state: &AppState, route: usize, backend: usize) -> bool;
pub fn spawn_warmup(state: AppState) -> Vec<tokio::task::JoinHandle<()>>;

// src/metrics.rs
impl Metrics {
    pub fn record_warmup(&self, route: usize, backend: usize, ok: bool);
}
```

YAML:

```yaml
warmup:
  interval_ms: 240000
```

Métricas:

| Métrica | Tipo | Etiquetas |
|---|---|---|
| `llm_gateway_backend_warm` | gauge | `route`, `backend` |
| `llm_gateway_warmup_total` | counter | `route`, `backend`, `result` (`success`/`failure`) |
