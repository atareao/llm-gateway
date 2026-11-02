# Tasks

## 1. RED — test que falla

- [x] 1.1 Crear `tests/common/mod.rs` con el `dead_backend()` **antiguo** (bind + `drop`) y `#![allow(dead_code)]`.
- [x] 1.2 Añadir en `tests/failover.rs` el test `dead_backend_port_is_not_reusable`: hace `bind` sobre la dirección devuelta y espera error.
- [x] 1.3 Ejecutar `cargo test --test failover dead_backend_port_is_not_reusable` y confirmar que **falla** (el puerto está libre).

## 2. GREEN — implementación mínima

- [x] 2.1 Reescribir `dead_backend()`: mantener el listener enlazado y lanzar una tarea que acepte y cierre cada conexión.
- [x] 2.2 Ejecutar el test de 1.2 y confirmar que pasa.
- [x] 2.3 Sustituir las copias locales de `dead_backend()` en `tests/failover.rs`, `tests/stream.rs`, `tests/integration.rs` y `tests/secrets.rs` por `mod common;` + `common::dead_backend()`.
- [x] 2.4 `cargo test` completo en verde.

## 3. REFACTOR

- [x] 3.1 `cargo fmt --check`.
- [x] 3.2 `cargo clippy --all-targets -- -D warnings` sin avisos (ojo con `dead_code` en `tests/common`).
- [x] 3.3 Ejecutar `cargo test` 10 veces seguidas y confirmar 0 fallos.

## 4. Docs

- [x] 4.1 Doc-comment en `dead_backend()` explicando por qué no se libera el puerto.

## 5. Git Flow

- [x] 5.1 Rama `feature/fix-flaky-dead-backend-tests` desde `development`.
- [x] 5.2 Commits con gitmoji + PR a `development`.
- [ ] 5.3 `openspec archive fix-flaky-dead-backend-tests` tras el merge.
