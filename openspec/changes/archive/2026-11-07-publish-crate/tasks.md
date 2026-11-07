# Tasks

## 1. Implementación

- [x] 1.1 `Cargo.toml`: `name = "llm-gateway-rs"`, añadir metadata (`description`, `license`, `repository`, `readme`, `keywords`, `categories`) y `[[bin]] name = "llm-gateway"`.
- [x] 1.2 Renombrar imports `llm_gateway::` → `llm_gateway_rs::` en `src/main.rs` y `tests/*.rs` (y doc-comments). NO tocar los strings de métricas `llm_gateway_*`.

## 2. Validación

- [x] 2.1 `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check`.
- [x] 2.2 `cargo publish --dry-run` OK (metadata y empaquetado válidos).
- [x] 2.3 Confirmar que las métricas siguen siendo `llm_gateway_*` (test de métricas en verde).

## 3. Cierre

- [x] 3.1 `openspec validate` y archivar.
- [ ] 3.2 PR → `development`; promoción a `main` para publicar `llm-gateway-rs` en crates.io.
