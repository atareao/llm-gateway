# Design: publish-crate

## Decisión: renombrar solo el paquete, no el binario

`[package] name = "llm-gateway-rs"` + `[[bin]] name = "llm-gateway"`. Así crates.io ve un nombre libre y los usuarios siguen invocando/descargando `llm-gateway` (y el archive del workflow no cambia).

## Decisión: renombrar la lib a `llm_gateway_rs`

El nombre de la lib por defecto deriva del paquete (`llm-gateway-rs` → `llm_gateway_rs`). Se actualizan los `use llm_gateway::` en `src/main.rs` y `tests/`. Alternativa descartada: `[lib] name = "llm_gateway"` para no tocar imports (menos explícito; se prefiere consistencia nombre-paquete/lib).

## Decisión: preservar nombres de métricas

Los strings `llm_gateway_*` (Prometheus) NO se tocan: son contrato observable. El reemplazo se limita a `llm_gateway::` (camino de crate).

## Verificación

- `cargo test`, `cargo clippy -D warnings`, `cargo fmt --check`.
- **`cargo publish --dry-run`** valida empaquetado y metadata sin subir (no consume el nombre ni la versión).
