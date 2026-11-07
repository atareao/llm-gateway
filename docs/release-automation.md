# Release automation

## Qué es

Automatización completa de las releases del proyecto: al mergear un PR `development` → `main`,
GitHub Actions **versiona, etiqueta, compila y publica** en una sola ejecución, sin pasos manuales.
Sustituye el flujo manual anterior (vampus + git-cliff + tag + `gh release`).

## Ficheros implicados

| Fichero | Rol |
|---|---|
| `.github/workflows/release.yml` | Workflow único autocontenido (todo el proceso). |
| `.vampus.yml` | `current_version`; vampus lo lee y actualiza. `replaces` apunta a `Cargo.toml`. |
| `cliff.toml` | Configuración de git-cliff (grupos y **preprocesadores de emojis**). |
| `Cargo.toml` / `Cargo.lock` | Versión del paquete (la bumpa vampus; el lock se sincroniza). |
| `CHANGELOG.md` | Regenerado por git-cliff en cada release. |
| `GIT_FLOW.md` | Documentación del flujo y de los secretos. |

## Cómo funciona

Trigger: `push` a `main` (también `workflow_dispatch` manual). `permissions: contents: write`
(usa el `GITHUB_TOKEN` del run; **no necesita `GH_PAT`**). `concurrency.group: release` evita
ejecuciones solapadas.

Jobs, en orden:

1. **`prepare`**
   - Guard anti-bucle: si el commit de cabecera empieza por `🚀 chore(release)`, se omite.
   - Detecta el **tipo de bump** desde los conventional commits entre el último tag y `HEAD`:
     - `BREAKING CHANGE` o `!:` → `--major`
     - algún `^feat` → `--minor`
     - resto → `--patch`
   - `vampus upgrade <tipo>` → actualiza `Cargo.toml` y `.vampus.yml`.
   - `cargo check --quiet` → sincroniza la versión en `Cargo.lock` antes del commit.
   - `git-cliff -o CHANGELOG.md --tag vX.Y.Z`.
   - Commit `🚀 chore(release): vX.Y.Z`, crea y empuja el tag `vX.Y.Z`, empuja a `main`.
   - Sincroniza `development` (`git push origin HEAD:development`, fast-forward).
   - Expone `outputs.version` y `outputs.released`.
2. **`build`** (matriz, `fail-fast: false`)
   - `x86_64-unknown-linux-gnu` (ubuntu-latest) y `aarch64-unknown-linux-gnu` (ubuntu-24.04-arm).
   - `cargo build --release --target …` y empaqueta `llm-gateway-<target>.tar.gz`.
3. **`release`**
   - Descarga los artefactos y crea la **GitHub Release** `vX.Y.Z` con las notas extraídas del `CHANGELOG.md`.
4. **`publish`**
   - `cargo publish --allow-dirty` **solo si** existe `CARGO_REGISTRY_TOKEN`; si falta, lo omite sin fallar.

Los jobs `build`/`release`/`publish` solo corren si `prepare.outputs.released == 'true'`.

## Variables / configuración

| Secreto | Requerido | Propósito |
|---|---|---|
| `CARGO_REGISTRY_TOKEN` | No | Token de crates.io. Si está definido, se publica el crate; si no, se omite y la release no falla. |

Requisitos de rama: `release.yml` empuja commit y tag a `main` y sincroniza `development`.
Si esas ramas tienen protecciones, hay que **permitir a GitHub Actions saltárselas** (o añadir
el bot a los bypass). No hace falta ningún PAT.

## Cómo habilitarlo / desplegarlo

1. Tener los workflows en `main` (ya presentes).
2. (Opcional) Definir el secreto `CARGO_REGISTRY_TOKEN` para publicar en crates.io:
   `crypta lookup cargo_registry_token | gh secret set CARGO_REGISTRY_TOKEN`.
3. (Si aplica) Permitir al bot de Actions empujar a `main`/`development`.
4. Flujo normal: PR `feature/*` → `development`; luego PR `development` → `main` dispara la release.

## Decisiones

- **Un solo workflow** (no `prepare` en un workflow y `build` en otro): un tag empujado con
  `GITHUB_TOKEN` **no** dispara otro workflow, así que encadenar por evento tag exigiría un PAT.
  Con jobs encadenados (`needs`) todo ocurre en la misma ejecución.
- **`GITHUB_TOKEN`** en lugar de `GH_PAT`: el repo se gestiona sin secretos obligatorios.
- **Emojis de git-cliff**: `cliff.toml` solo reconoce los emojis listados en `commit_preprocessors`.
  Un commit con un emoji no listado (p. ej. `📦`) se trata como **no convencional** y **se descarta
  del changelog** (`filter_unconventional = true`). Usar solo los emojis de la tabla de `GIT_FLOW.md`.

## Follow-ups

- Publicar también binarios para macOS/Windows si se necesita distribución multiplataforma.
- Notificar (webhook/Slack) al publicar una release.
