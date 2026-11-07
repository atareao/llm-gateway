# Design: release-automation

## Contexto

Repo Rust (crate `llm-gateway`, lib+bin, edition 2024) con gitflow; ya existen `.vampus.yml` (current_version 0.3.0, `replaces` sobre `Cargo.toml`) y `cliff.toml`. Faltan los dos workflows que `GIT_FLOW.md` describe. `ci.yml` corre en PR a `main`/`development`.

## Decisión: dos workflows separados

- **`release-prepare.yml`** (push a `main`): responsabilidad de *versionar*. Bump + changelog + commit + tag + sync `development`. Ignora sus propios commits de release para no entrar en bucle.
- **`release.yml`** (push de tag `v*`): responsabilidad de *artefactos*. Compila y publica. Separar permite reintentar builds sin re-versionar y mantiene cada workflow con un trigger claro.

## Decisión: detección de bump en `release-prepare`

Se calcula desde los commits entre el último tag y `HEAD`:
- `BREAKING CHANGE` o `!:` → `--major`
- algún `^feat` → `--minor`
- resto → `--patch`

Es la misma lógica documentada en `GIT_FLOW.md`. El guard `if: "!startsWith(head_commit.message, '🚀 chore(release)')"` evita bucles.

## Decisión: sincronizar `development` con `main`

Tras commit+tag en `main`, se hace `git push origin HEAD:development`. Como el release branch se creó desde `development`, es un fast-forward (no se pierde nada). Se descarta el `gh api -X PUT ... -f sha=` en modo force del template de la skill (reescribe la ref y podría descartar commits si `development` avanzara entre medias).

## Decisión: actualizar `Cargo.lock`

Tras `vampus upgrade` (que solo toca `Cargo.toml`), se ejecuta `cargo check --quiet` para que Cargo sincronice la versión del paquete en `Cargo.lock` antes del commit. Así el lock no queda desfasado.

## Decisión: build multiplataforma con rustls

`reqwest` usa `rustls-tls` → sin dependencia de OpenSSL. Los targets `x86_64-unknown-linux-gnu` y `aarch64-unknown-linux-gnu` se construyen con `cargo build --release --target`. No se añaden `libssl`/`alsa`. Empaquetado `tar.gz` por target.

## Decisión: publicación en crates.io opcional

Un job `check-token` expone `has_token=${{ secrets.CARGO_REGISTRY_TOKEN != '' }}`; el job `publish` corre solo si es `true`. Evita que la release falle en repos que no publican en crates.io, pero la habilita sin tocar el workflow cuando exista el secreto.

## Riesgos

- `GH_PAT` debe existir con `contents: write`; sin él, el push del tag/commit falla (documentado en `GIT_FLOW.md`).
- El push a `main` desde un workflow puede requerir permitir a Actions saltarse las protecciones de rama; se documenta.
- Determinismo del changelog: depende de `cliff.toml` ya presente.
