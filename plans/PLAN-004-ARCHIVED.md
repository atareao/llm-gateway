# PLAN-004 — Documentar features de release automation y publish-crate

## Objetivo

Cerrar el hueco de documentación de las dos features de tooling recién cerradas, cumpliendo `AGENTS.md` §IX (`docs/<feature>.md` por feature), y añadir badge/instalación de crates.io al README.

## Entregables

- `docs/release-automation.md` — workflow único `release.yml` (bump, changelog, tag, build, GitHub Release, crates.io), secretos, decisiones y trampa del emoji de `cliff.toml`.
- `docs/publish-crate.md` — renombrado del paquete a `llm-gateway-rs`, metadata, binario `llm-gateway`, imports, invariantes (métricas).
- `README.md` — badge de crates.io y sección de instalación (`cargo install llm-gateway-rs`).

## Naturaleza

- Solo documentación: sin cambios en `src/`, tests, CI ni dependencias. Sin cambio de comportamiento → no requiere change OpenSpec.

## Estado

- [x] `docs/release-automation.md`
- [x] `docs/publish-crate.md`
- [x] `README.md` (badge + install)
- [x] PR → `development`
