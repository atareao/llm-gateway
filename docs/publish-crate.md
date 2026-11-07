# publish-crate

## Qué es

Renombrado del **paquete** a `llm-gateway-rs` y adición de la metadata necesaria para publicarlo
en **crates.io**, manteniendo el **binario** como `llm-gateway`. Habilita el job `publish` del
workflow de release.

## Por qué

El nombre `llm-gateway` **ya estaba ocupado en crates.io** por otro usuario (`lipish`,
repo `xinference/llm-gateway`), y al `Cargo.toml` le faltaban `description`/`license`
(obligatorios). Publicar con ese nombre habría fallado y tumbado la release.

## Ficheros implicados

| Fichero | Cambio |
|---|---|
| `Cargo.toml` | `name = "llm-gateway-rs"`, `autobins = false`, `[[bin]] name = "llm-gateway"`, metadata (`description`, `license`, `repository`, `readme`, `keywords`, `categories`). |
| `Cargo.lock` | Renombra el paquete a `llm-gateway-rs`. |
| `src/main.rs`, `tests/*.rs` | Imports `llm_gateway::` → `llm_gateway_rs::`. |
| `.github/workflows/release.yml` | Job `publish` (ya existente) publica el crate si hay token. |

## Cómo funciona

- `Cargo.toml`:
  ```toml
  [package]
  name = "llm-gateway-rs"
  autobins = false
  license = "MIT"
  # description, repository, readme, keywords, categories …

  [[bin]]
  name = "llm-gateway"
  path = "src/main.rs"
  ```
- Al desactivar los bins automáticos y declarar `[[bin]]` explícito, el **paquete** se llama
  `llm-gateway-rs` pero el **ejecutable** sigue siendo `llm-gateway` (y los `tar.gz` no cambian).
- El nombre de la librería deriva del paquete → `llm_gateway_rs`; de ahí el renombrado de imports.
- Instalación:
  ```bash
  cargo install llm-gateway-rs   # instala el binario `llm-gateway`
  ```

## Invariantes

- **Los nombres de métricas Prometheus NO cambian**: siguen siendo `llm_gateway_*` (contrato observable).
  El reemplazo se limitó a `llm_gateway::` (camino de crate), nunca a los strings de métricas.
- El **binario y los archivos de distribución** siguen llamándose `llm-gateway`.

## Verificación

```bash
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
cargo publish --dry-run --allow-dirty   # valida empaquetado y metadata sin subir
```

## Decisiones

- Renombrar **solo el paquete**, no el binario → crates.io ve un nombre libre y la distribución no cambia.
- Renombrar la lib a `llm_gateway_rs` (consistencia nombre-paquete/lib) en vez de forzar
  `[lib] name = "llm_gateway"` para no tocar los imports.
- Publicación **condicionada** al secreto `CARGO_REGISTRY_TOKEN`: sin él, la release no falla.

## Follow-ups

- Añadir `homepage`/`documentation` a la metadata si se quiere enriquecer la ficha de crates.io.
- Badge de crates.io en el README (ver `README.md`).
