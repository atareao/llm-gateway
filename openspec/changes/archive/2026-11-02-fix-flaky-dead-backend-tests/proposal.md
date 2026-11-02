# Proposal: fix-flaky-dead-backend-tests

## Intent

Eliminar el fallo intermitente de `primary_down_fails_over_to_fallback` (y de cualquier test que use `dead_backend()`), causado por una carrera de puertos efímeros en el helper de tests.

## Problem

`dead_backend()` hace `TcpListener::bind("127.0.0.1:0")`, lee el puerto y hace `drop(listener)`. Entre el `drop` y la petición del gateway, otro test del mismo binario puede enlazar ese mismo puerto efímero. El "primario caído" pasa a estar vivo y responde `404` a `/v1/chat/completions`; el gateway propaga el 4xx (no es `429`) y el test falla con `left: 404, right: 200`.

El helper está duplicado en `tests/failover.rs`, `tests/stream.rs`, `tests/integration.rs` y `tests/secrets.rs`.

## Scope

**In:**
- Nuevo helper compartido `tests/common/mod.rs` con un `dead_backend()` determinista.
- Sustituir las cuatro copias locales por el helper compartido.
- Test de caracterización que demuestra que el puerto devuelto no es reutilizable.

**Out:**
- Cambios en `src/`: el gateway no tiene ningún bug, el fallo es del arnés de tests.
- Reintentos, backoff y clasificación fina de errores (van en otro change).

## Approach

`dead_backend()` mantiene el `TcpListener` enlazado y lanza una tarea que acepta conexiones y las cierra inmediatamente (`drop(stream)`). El cliente ve un error de conexión (conexión cerrada sin respuesta) en lugar de un `ECONNREFUSED`, que es exactamente el fallo que el gateway debe tratar como "backend no disponible". El puerto nunca se libera, así que ningún otro test puede robarlo.

## Impact

- Solo tests. Sin cambios de comportamiento en producción.
- `cargo test` deja de ser flaky.

## Contracts

```rust
// tests/common/mod.rs
pub async fn dead_backend() -> String; // "http://127.0.0.1:<puerto>/v1"
```
