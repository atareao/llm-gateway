# PLAN-009 — Follow-ups de rutas dinámicas OpenRouter free

- **Issue:** #32 (follow-ups no bloqueantes de PLAN-008 / #31)
- **Change OpenSpec:** `openspec/changes/add-openrouter-free-followups/`
- **Rama:** `chore/plan-008-followups` (desde `development`)
- **Estado:** pendiente de aprobación del spec (STOP SDD)

## Objetivo

Cerrar los follow-ups de rendimiento, configurabilidad, observabilidad y tests detectados en la auditoría de PLAN-008, sin cambiar el comportamiento externo del gateway ni romper configuraciones existentes.

## Alcance

| ID | Tipo | Cambio | Ficheros |
|---|---|---|---|
| M5 | Rendimiento | Candidato ligero en lugar de clonar `Backend` por modelo expandido | `src/proxy.rs` |
| N3 | Rendimiento | Deduplicar `eligible_model_ids` con `HashSet`; índice `(route, backend)` | `src/discovery.rs` |
| Gauge | Métricas | Escribir el gauge de blacklist solo si cambia el tamaño | `src/discovery.rs`, `src/metrics.rs` |
| M6 | Config | TTL único validado por `base_url` (error si hay conflicto) | `src/config.rs`, `src/discovery.rs` |
| N1 | Config | `Discovery` con defaults por campo vía `#[serde(default)]` | `src/config.rs` |
| N2 | Observabilidad | Warning ante `include` no elegible | `src/classify.rs`, `src/discovery.rs` |
| N4 | Tests | Eliminar carrera `set_var`/`getenv` | `tests/dynamic_routing.rs` |
| Cobertura | Tests | Catálogo vacío, TTL vencido + fallo de red, blacklist compartida | `tests/dynamic_routing.rs`, `src/discovery.rs` |

## No objetivos

- No se toca el esquema de configuración existente más allá de defaults y validación de TTL.
- No se cambia el formato de las métricas expuestas ni los endpoints.
- No se modifica `~/.config/opencode/gateway.yaml`.

## Contratos

- `Discovery` mantiene sus campos pero `refresh_interval_ms` y `blacklist_cooldown_ms` pasan a tener default; `0` explícito sigue siendo error.
- `Candidate` en `src/proxy.rs` queda como `{ slot: usize, model: String, base_url: String, timeout_ms: Option<u64>, api_key_env: Option<String>, dynamic_cooldown: Option<Duration> }`.
- `GatewayConfig::validate` valida que todos los backends dinámicos con la misma `base_url` comparten `refresh_interval_ms` (o el default); `Registry::new` solo se beneficia de esa validación.
- `classify` recibe el contexto necesario (`base_url`) para emitir el warning de `include` ignorado.
- `Blacklist` recuerda el último tamaño publicado y evita reescrituras del gauge.

## Escenarios clave (ver delta OpenSpec para el detalle Given/When/Then)

1. `discovery: { min_context: 32768 }` y `discovery: {}` cargan con defaults.
2. Dos backends dinámicos con la misma `base_url` y TTLs distintos producen error de config con la `base_url` y las rutas.
3. `include` no elegible no entra en la lista y emite warning con modelo, perfil y `base_url`.
4. La expansión de rutas dinámicas no clona el `Backend` completo y conserva modelo, `base_url`, timeout y `api_key_env`.
5. `/v1/models` no devuelve identificadores duplicados.
6. El gauge de blacklist solo se reescribe cuando cambia el tamaño.
7. Los tests de `dynamic_routing` son deterministas en ejecución repetida.
8. Catálogo vacío, TTL vencido con fallo de red y blacklist compartida entre rutas quedan cubiertos.

## Fases (TDD)

1. **Baseline:** suite y linters en verde antes de tocar nada.
2. **RED:** tests por cada follow-up en el orden de `tasks.md` (config → clasificación → proxy → discovery → métricas → tests).
3. **GREEN:** implementación mínima por follow-up.
4. **REFACTOR:** limpieza de firmas y clones, sin cambio de comportamiento.
5. **Docs:** `docs/openrouter-free-followups.md` y actualización de `docs/openrouter-free-routing.md`.
6. **Verificación:** `cargo test`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo fmt --check`, `openspec validate --strict`.

## Delegación

- Implementación Rust: `@backend-dev`.
- Auditoría: `@rust-reviewer` tras cada bloque funcional.
- Verificación de plan: `@qa-plan-verifier` antes de cerrar.

## Criterio de cierre

- Todos los escenarios del delta tienen test y pasan.
- Sin warnings de Clippy ni cambios de comportamiento en los tests preexistentes.
- `tasks.md` completo, change archivado con `openspec archive add-openrouter-free-followups`, este plan renombrado a `plans/PLAN-009-ARCHIVED.md` y PR a `development`.