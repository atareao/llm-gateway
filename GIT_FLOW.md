# Git Flow

Este proyecto sigue **Git Flow** con versionado semántico automático.

## Ramas

| Rama | Propósito | Base |
|---|---|---|
| `main` | Producción. Cada merge aquí dispara una release automática. | — |
| `development` | Integración de features en curso. | `main` |
| `feature/*` | Nuevas funcionalidades. | `development` |
| `hotfix/*` | Correcciones urgentes a producción. | `main` |

## Flujo diario

### Features

```bash
git checkout development && git pull origin development
git checkout -b feature/mi-feature
git commit -m "✨ feat: add dark mode toggle"
git push origin feature/mi-feature
# Crear Pull Request en GitHub
```

### Hotfixes

```bash
git checkout main && git pull origin main
git checkout -b hotfix/arreglo-critico
git commit -m "🚑️ hotfix: crash on empty input"
git push origin hotfix/arreglo-critico
# Crear Pull Request en GitHub
# Después del merge, sincronizar development
git checkout development && git merge main && git push origin development
```

## Conventional Commits con Gitmoji

| Mensaje | Bump |
|---|---|
| `✨ feat: ...` | minor (0.Y.0) |
| `🐛 fix: ...` | patch (0.0.Z) |
| `♻️ refactor: ...` | patch (0.0.Z) |
| `📝 docs: ...` | patch (0.0.Z) |
| `💥 feat!: ...` o `BREAKING CHANGE` | major (X.0.0) |

### Gitmoji de referencia

| Tipo | Emoji |
|---|---|
| feat | ✨ |
| fix | 🐛 |
| hotfix | 🚑️ |
| docs | 📝 |
| refactor | ♻️ |
| perf | ⚡ |
| style | 💄 |
| test | ✅ |
| chore | 🔧 |
| ci | 👷 |
| revert | ⏪️ |
| deps | ⬆️ |
| breaking | 💥 |

## CI Workflows

- **`ci.yml`** — push y PR a `main`/`development`: formato, lint, build, tests.
- **`release.yml`** — push a `main` (**workflow único, sin `GH_PAT`**): detecta el bump
  (major/minor/patch) desde los conventional commits, ejecuta `vampus upgrade`,
  regenera `CHANGELOG.md` con `git-cliff`, hace commit `🚀 chore(release): vX.Y.Z`,
  crea y empuja el tag `vX.Y.Z`, sincroniza `development`, compila los binarios Linux
  (`x86_64` y `aarch64`) y crea la GitHub Release con las notas del `CHANGELOG.md` y los
  `tar.gz`. Publica en crates.io solo si existe `CARGO_REGISTRY_TOKEN`.

### Releases (automático)

```bash
# 1. PR de development → main (conventional commits definen el bump)
# 2. Al mergear, release.yml versiona, etiqueta, compila y publica en una sola ejecución.
```

Igual que antes, sigue siendo válido hacerlo a mano (vampus + git-cliff + tag + `gh release`).

## Secretos de GitHub necesarios

| Secreto | Propósito |
|---|---|
| `CARGO_REGISTRY_TOKEN` | *(opcional)* Token de crates.io; si está definido, `release.yml` publica el crate. Sin él, la release no falla: simplemente no publica. |

> Notas:
> - `release.yml` usa el `GITHUB_TOKEN` del propio workflow (`permissions: contents: write`);
>   **no requiere `GH_PAT`**.
> - Empuja commit y tag a `main` y sincroniza `development`: si esas ramas tienen
>   protecciones, permite a GitHub Actions saltárselas (o añade el bot a los bypass).
