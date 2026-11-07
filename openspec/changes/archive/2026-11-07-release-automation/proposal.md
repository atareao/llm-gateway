# Proposal: release-automation

## Why

`GIT_FLOW.md` documenta una release automática al mergear a `main` (bump de versión, changelog, tag y publicación), pero el repo solo tiene `.github/workflows/ci.yml`. La release `v0.3.0` tuvo que hacerse a mano (vampus + git-cliff + tag + `gh release`). Automatizar elimina ese trabajo manual y el riesgo de errores.

## What Changes

- Añadir `.github/workflows/release-prepare.yml` — en push a `main`: detecta el tipo de bump desde los conventional commits, ejecuta `vampus upgrade`, regenera `CHANGELOG.md` con `git-cliff`, hace commit `🚀 chore(release): vX.Y.Z`, crea y empuja el tag `vX.Y.Z`, y sincroniza `development`.
- Añadir `.github/workflows/release.yml` — en push de tag `v*`: compila binarios Linux `x86_64` y `aarch64`, crea/actualiza la GitHub Release adjuntando los artefactos, y publica en crates.io **solo si** existe el secreto `CARGO_REGISTRY_TOKEN`.
- Actualizar `GIT_FLOW.md` para reflejar los workflows reales y los secretos necesarios.

## Capabilities

### New Capabilities
<!-- Ninguna: es tooling de CI, sin cambio de comportamiento del gateway. skip_specs: true. -->

### Modified Capabilities
<!-- Ninguna. -->

## Impact

- Solo CI/CD y documentación. Sin cambios en `src/`, tests ni dependencias.
- Secretos requeridos: `GH_PAT` (contents: write) y, opcional, `CARGO_REGISTRY_TOKEN`.
- El binario es `llm-gateway` (crate lib+bin, edition 2024); usa `rustls-tls`, así que no necesita `libssl`/`pkg-config`.
