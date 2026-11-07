# Proposal: publish-crate

## Why

El workflow `release.yml` ya intenta publicar en crates.io, pero el nombre del paquete `llm-gateway` **ya está ocupado en crates.io por otro usuario** (`lipish`) y al `Cargo.toml` le faltan `description`/`license` (obligatorios). Publicar así fallaría y tumbaría la release.

## What Changes

- Renombrar el paquete a **`llm-gateway-rs`** (nombre libre en crates.io). El **binario sigue llamándose `llm-gateway`** vía `[[bin]] name = "llm-gateway"`, para no cambiar la distribución ni los `tar.gz`.
- Renombrar la librería a `llm_gateway_rs` y actualizar los `use llm_gateway::` → `use llm_gateway_rs::` en `src/` y `tests/`.
- Añadir metadata de publicación: `description`, `license = "MIT"`, `repository`, `readme`, `keywords`, `categories`.
- **No** cambiar los nombres de las métricas Prometheus (`llm_gateway_*`): son parte de la interfaz observable.

## Capabilities

### New Capabilities
<!-- Ninguna: cambio de empaquetado (skip_specs: true). -->

### Modified Capabilities
<!-- Ninguna: el comportamiento del gateway no cambia. -->

## Impact

- `Cargo.toml` (name, metadata, `[[bin]]`), `src/main.rs`, `tests/*.rs` (imports), doc-comments.
- Sin cambios de comportamiento ni de métricas. Sin dependencias nuevas.
- Habilita el job `publish` de `release.yml` con el secreto `CARGO_REGISTRY_TOKEN`.
