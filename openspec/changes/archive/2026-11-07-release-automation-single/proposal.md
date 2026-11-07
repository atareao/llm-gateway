# Proposal: release-automation-single

## Why

El diseño de dos workflows (`release-prepare.yml` + `release.yml`) requiere el secreto `GH_PAT`, porque un tag empujado con `GITHUB_TOKEN` no dispara otro workflow. El repo no tiene secretos, así que la automatización quedaría inoperante sin configuración manual. Un único workflow autocontenido con `GITHUB_TOKEN` (`contents: write`) funciona sin secretos.

## What Changes

- Sustituir los dos workflows por **uno solo** `.github/workflows/release.yml` disparado en push a `main` (y `workflow_dispatch`) que, en la misma ejecución: detecta el bump, ejecuta `vampus upgrade`, regenera `CHANGELOG.md` con `git-cliff`, commitea `🚀 chore(release): vX.Y.Z`, crea y empuja el tag, sincroniza `development`, compila los binarios Linux y crea la GitHub Release.
- **Eliminar** `.github/workflows/release-prepare.yml`.
- Publicar en crates.io **solo si** existe `CARGO_REGISTRY_TOKEN` (omitido en caso contrario, sin fallar).
- Actualizar `GIT_FLOW.md` (un workflow, sin `GH_PAT`).

## Capabilities

### New Capabilities
<!-- Ninguna: tooling de CI (skip_specs: true). -->

### Modified Capabilities
<!-- Ninguna. -->

## Impact

- Solo CI/CD y documentación. Sin cambios en `src/`, tests ni dependencias.
- Ya no requiere `GH_PAT`; usa `GITHUB_TOKEN` con `permissions: contents: write`.
- Requiere que la rama `main` permita push del bot (o bypass de protecciones); se documenta.
