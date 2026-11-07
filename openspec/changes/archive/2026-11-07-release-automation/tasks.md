# Tasks

## 1. Workflows

- [x] 1.1 Crear `.github/workflows/release-prepare.yml` (bump vampus + changelog git-cliff + commit + tag + push main + sync development, con guard anti-bucle y sync de `Cargo.lock`).
- [x] 1.2 Crear `.github/workflows/release.yml` (matriz `x86_64`/`aarch64` Linux, `cargo build --release --target`, empaquetado `tar.gz`, GitHub Release con artefactos; job `publish` a crates.io condicionado a `CARGO_REGISTRY_TOKEN`).

## 2. Validación

- [x] 2.1 Validar sintaxis YAML de ambos workflows (`python -c yaml.safe_load`).
- [x] 2.2 Revisar coherencia con `.vampus.yml`, `cliff.toml`, `ci.yml` (triggers, nombres de fichero, secretos).

## 3. Docs

- [x] 3.1 Actualizar `GIT_FLOW.md`: workflows reales, secretos y nota sobre protecciones de rama / `GH_PAT`.

## 4. Cierre

- [x] 4.1 `openspec validate release-automation` y archivar el change.
- [x] 4.2 PR `chore/release-automation` → `development`.
