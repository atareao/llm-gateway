---
# PLAN-003 — Automatización de releases (ARCHIVED)

## Objetivo

Automatizar el versionado y la publicación eliminando el paso manual de la release v0.3.0.

## Entregables

- `.github/workflows/release-prepare.yml`: bump (vampus) + changelog (git-cliff) + commit + tag `vX.Y.Z` + sync `development`, en push a `main`.
- `.github/workflows/release.yml`: build `x86_64`/`aarch64` Linux + GitHub Release con `tar.gz`, y publish a crates.io opcional, en tag `v*`.
- `GIT_FLOW.md` actualizado (workflows y secretos).

## SDD

- Change `release-automation` (`skip_specs: true`), archivado en `openspec/changes/archive/2026-11-07-release-automation/`.

## Estado

- [x] Workflows creados y validados (YAML OK).
- [x] Docs actualizadas.
- [x] Change archivado.
- [x] PR `chore/release-automation` → `development`.

## Follow-ups

- Definir `GH_PAT` en el repo y permitir a Actions saltar protecciones de rama si aplica.
- `CARGO_REGISTRY_TOKEN` solo si se quiere publicar en crates.io.
