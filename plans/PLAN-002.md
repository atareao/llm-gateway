---
# PLAN-002 — Warmup hardening

## Objetivo

Corregir los dos defectos del keep-alive detectados en la verificación real (5.1) y pulir el resto de hallazgos del reviewer: eliminar la ráfaga de warmups tras un warmup lento (`MissedTickBehavior::Delay`), hacer configurable el timeout de warmup (`timeout_ms`), resolver la API key una vez por tarea (sin spam de `warn`), registrar la causa de los fallos de transporte y añadir apagado ordenado de las tareas.

## No-objetivos

- Cache/KV caching: fuera de alcance.
- Reintentos/failover específicos del warmup.
- Control nativo de `keep_alive` de Ollama.

## Artefactos canónicos (SDD)

- Change OpenSpec: `openspec/changes/warmup-hardening/` (proposal, design, specs/gateway/spec.md, tasks).
- Spec vigente: `openspec/specs/gateway/spec.md` (se modifica vía delta al archivar).
- Tareas TDD: ver `openspec/changes/warmup-hardening/tasks.md`.

## Diseño (resumen)

- `Warmup { interval_ms, timeout_ms: Option<u64> }`; default timeout `WARMUP_TIMEOUT` (30 s); validación `> 0`.
- `spawn_warmup`: `MissedTickBehavior::Delay`, guardia `interval.is_zero()`, clave resuelta una vez.
- `warmup_once`: timeout por backend + log de causa de error de transporte.
- `main.rs`: guardar `JoinHandle` y abortarlos en SIGINT/SIGTERM (`with_graceful_shutdown`).

## Verificación

- `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`.
- 5.1 con Ollama real (`qwen3:14b`, `timeout_ms` ~90-300 s): arranque `success` sin fallo por timeout y sin ráfaga.

## Estado

- [ ] Change `warmup-hardening` aprobado por el usuario.
- [ ] RED → GREEN → REFACTOR.
- [ ] Docs actualizadas → `docs/warmup-hardening.md`.
- [ ] Change archivado y plan renombrado a `plans/PLAN-002-ARCHIVED.md`.
