# Tasks

## 1. Implementación

- [x] 1.1 Reescribir `.github/workflows/release.yml` como workflow único autocontenido (prepare → build → release/publish) con `GITHUB_TOKEN`.
- [x] 1.2 Eliminar `.github/workflows/release-prepare.yml`.

## 2. Validación

- [x] 2.1 Validar YAML (`python yaml.safe_load`).
- [x] 2.2 Revisar coherencia con `.vampus.yml`, `cliff.toml`, `ci.yml`.

## 3. Docs

- [x] 3.1 Actualizar `GIT_FLOW.md`: un solo workflow, sin `GH_PAT`, nota de protecciones de rama.

## 4. Cierre

- [ ] 4.1 `openspec validate` y archivar.
- [ ] 4.2 PR → `development`, merge y promoción a `main` para verificar la automatización en vivo.
