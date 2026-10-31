//! Biblioteca del gateway: expone los módulos reutilizables por el binario y
//! por los tests de integración en `tests/`.
//!
//! Decisión de diseño: se usa `lib.rs` además de `main.rs` para que los tests
//! de integración (`tests/`) puedan ejercitar la API pública (`config::load`)
//! sin duplicar lógica. `main.rs` queda como capa fina de arranque.

pub mod config;
