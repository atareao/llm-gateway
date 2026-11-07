# Design: release-automation-single

## Contexto

`release-automation` introdujo dos workflows que dependían de `GH_PAT` para encadenar tag → build. Sin secretos en el repo, no funcionan. Se unifica.

## Decisión: un único workflow con jobs encadenados

Jobs: `prepare` → `build` (matriz) → `release`/`publish`. Todo en la misma ejecución de push a `main`, de modo que no hace falta que un evento dispare otro workflow; el tag solo identifica la versión y sirve de `ref` para el build (vía `needs.prepare.outputs.version`).

## Decisión: `GITHUB_TOKEN` en lugar de `GH_PAT`

`permissions: contents: write` da al token del run permiso de push a `main`, tag y `development`. Se elimina la dependencia de un PAT. El guard `!startsWith(head_commit.message, '🚀 chore(release)')` mantiene la idempotencia; además, un push hecho con `GITHUB_TOKEN` no re-dispara el workflow (by design de GitHub), evitando bucles.

## Decisión: sync de `Cargo.lock`

Tras `vampus upgrade`, `cargo check --quiet` sincroniza la versión del paquete en `Cargo.lock` antes del commit.

## Decisión: publish condicional

El job `publish` comprueba `CARGO_REGISTRY_TOKEN` en tiempo de ejecución; si está vacío, omite `cargo publish` y termina OK. Así el workflow no falla en repos sin crates.io.

## Riesgos

- Push a `main`/`development` con `GITHUB_TOKEN` falla si hay protecciones de rama que lo impidan: hay que permitir al bot (o bypass). Documentado.
- El build multiplataforma usa `rustls-tls` (sin OpenSSL); targets `x86_64`/`aarch64` Linux.
