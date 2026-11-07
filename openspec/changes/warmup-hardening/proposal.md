# Proposal: warmup-hardening

## Why

La verificación 5.1 contra un Ollama real destapó dos defectos del keep-alive que los mocks no cubrían: (1) un **burst** de warmups acumulados tras un warmup lento (por `MissedTickBehavior::Burst`), y (2) el **`WARMUP_TIMEOUT` fijo de 30 s se queda corto** ante el cold load real de un modelo grande (~90 s para un 14B), de modo que los primeros warmups fallan por timeout. Además hay ruido de logs y falta de diagnóstico en el fallo de transporte.

## What Changes

- `Warmup` gana un campo opcional `timeout_ms: Option<u64>`; si se omite, se usa el default `WARMUP_TIMEOUT` (30 s). Permite fijar un timeout mayor para backends locales con modelos grandes.
- `spawn_warmup` usa `MissedTickBehavior::Delay`: tras un warmup que dure más que `interval_ms`, no se dispara una ráfaga de ticks acumulados.
- La API key se resuelve **una vez** al arrancar la tarea de keep-alive (no en cada tick); si `api_key_env` no resuelve, se emite **un solo** `warn` y **un solo** `failure`, y el bucle continúa en `debug`.
- El fallo de transporte del warmup registra la causa (`reqwest::Error`) en el log, no solo `result="failure"`.
- `main.rs` conserva los `JoinHandle` de `spawn_warmup` y los **aborta en un apagado ordenado** (SIGINT/SIGTERM).
- Validación de config: `timeout_ms`, si está presente, debe ser `> 0`.
- Hardening defensivo: `spawn_warmup` ignora (`continue`) un `interval_ms == 0` en lugar de paniquear por precondición.

## Capabilities

### New Capabilities
<!-- Ninguna: se modifica la capacidad existente `gateway`. -->

### Modified Capabilities

- `gateway`: cambia el comportamiento de keep-alive (sin burst, timeout configurable, resolución única de clave, diagnóstico de fallos) y añade el apagado ordenado de las tareas.

## Impact

- `src/config.rs`: campo `Warmup.timeout_ms` + validación `> 0`.
- `src/warmup.rs`: `MissedTickBehavior::Delay`, timeout por backend, resolución de clave única, logging de causa de error, guardia `interval.is_zero()`.
- `src/main.rs`: guardar y abortar los `JoinHandle` en el cierre.
- `src/proxy.rs`: exponer un helper de resolución de clave sin el `warn` repetido (o parámetro de quietud) para el camino de warmup.
- `tests/warmup.rs`, `tests/config.rs`: nuevos escenarios.
- Docs: `docs/backend-warmup.md`, `gateway.yaml.example`, `README.md`.
- Sin dependencias nuevas.

## Contracts

```rust
// src/config.rs
pub struct Warmup {
    pub interval_ms: u64,          // > 0
    pub timeout_ms: Option<u64>,   // > 0 si presente; default 30_000
}
pub struct Backend { /* ... */ pub warmup: Option<Warmup> }

// src/warmup.rs
pub const WARMUP_TIMEOUT: Duration; // 30 s (default)
pub async fn warmup_once(state: &AppState, route: usize, backend: usize) -> bool;
pub fn spawn_warmup(state: AppState) -> Vec<tokio::task::JoinHandle<()>>;
```

YAML:

```yaml
warmup:
  interval_ms: 240000
  timeout_ms: 300000   # opcional; default 30 s
```
