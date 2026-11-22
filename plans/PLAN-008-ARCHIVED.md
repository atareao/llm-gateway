# PLAN-008 — Rutas dinámicas OpenRouter free por perfil (architect / code / writer)

- **Issue:** #31 (https://github.com/atareao/llm-gateway/issues/31)
- **Change OpenSpec:** `openspec/changes/add-openrouter-free-routing`
- **Rama:** `feature/add-openrouter-free-routing` (desde `development`)
- **Estado:** BORRADOR — pendiente de aprobación de la spec por el usuario
- **Fecha:** 2026-11-22
- **Bloqueado por:** nada (PLAN-007 ya mergeado, PR #30)

## 1. Objetivo

Añadir al gateway **rutas dinámicas de modelos gratuitos de OpenRouter**, especializadas por **perfil de uso** (`architect`, `code`, `writer`), con descubrimiento automático del catálogo free, failover, rotación round-robin y blacklist dinámica, y fallback a backends de pago declarados por ruta. Sustituye la idea inicial de una única ruta dinámica genérica.

## 2. Alcance

**Incluye:**
- Nuevo `kind: openrouter_free` en `Backend` con `profile: architect | code | writer` (default `code`), mezclable con la lista fija de backends.
- Descubrimiento del catálogo free (`GET {base_url}/models`, público) con filtro de tool-calling y caché con TTL.
- Clasificación por perfil (heurística + overrides).
- Expansión de backends dinámicos en su posición con rotación round-robin, reutilizando `route_with_failover`.
- Blacklist dinámica **global por modelo** (429/5xx/Retry-After) con cooldown.
- Fallback a backends de pago; `AllBackendsFailed` si se agota la ruta.
- Métricas `llm_gateway_discovery_*` en OTLP/Prometheus.
- `docs/openrouter-free-routing.md`.

**No incluye:**
- Endpoint admin para inspeccionar catálogo/perfiles/blacklist.
- Benchmarks propios de calidad.
- Descubrimiento de proveedores distintos de OpenRouter.

## 3. Decisiones acordadas con el usuario

1. **Tres perfiles**: `architect`, `code`, `writer` (no una sola ruta dinámica).
2. **`architect` = razonamiento primero, general grande como respaldo.** Orden por tiers: tier 0 razonamiento (`r1`, `reasoner`, `thinking`, `qwq`, `o[0-9]`), tier 1 general grande (`llama-3.3-70b`, `qwen-*72b`, `deepseek-v3`…), tier 2 resto; dentro de cada tier `context_length` desc y luego `id`.
3. **Un solo descubrimiento, tres clasificaciones** (un único `/models`, un único TTL; no tres descubridores).
4. **Blacklist global por modelo** (un 429 es por modelo+clave), compartida entre rutas.
5. **Compatibilidad total**: `kind` por defecto `static`.

## 4. Configuración de ejemplo (tres rutas)

```yaml
route:
  - name: architect
    backend:
      - kind: openrouter_free
        profile: architect
        base_url: "https://openrouter.ai/api/v1"
        api_key_env: "OPENROUTER_API_KEY"
        discovery:
          refresh_interval_ms: 300000
          blacklist_cooldown_ms: 60000
          min_context: 32768
      - base_url: "https://openrouter.ai/api/v1"   # fallback de PAGO (razonamiento)
        model: "<modelo-pago-razonamiento>"
        api_key_env: "OPENROUTER_API_KEY"

  - name: code
    backend:
      - kind: openrouter_free
        profile: code
        base_url: "https://openrouter.ai/api/v1"
        api_key_env: "OPENROUTER_API_KEY"
        discovery:
          refresh_interval_ms: 300000
          blacklist_cooldown_ms: 60000
          min_context: 32768
      - base_url: "https://openrouter.ai/api/v1"   # fallback de PAGO (coder)
        model: "<modelo-pago-coder>"
        api_key_env: "OPENROUTER_API_KEY"

  - name: writer
    backend:
      - kind: openrouter_free
        profile: writer
        base_url: "https://openrouter.ai/api/v1"
        api_key_env: "OPENROUTER_API_KEY"
        discovery:
          refresh_interval_ms: 300000
          blacklist_cooldown_ms: 60000
          min_context: 32768
      - base_url: "https://openrouter.ai/api/v1"   # fallback de PAGO (prosa)
        model: "<modelo-pago-prosa>"
        api_key_env: "OPENROUTER_API_KEY"
```

## 5. Puntos de extensión en código

- `src/config.rs`: `Backend` (nuevo `kind`, `profile`, `discovery`), nuevos enums `BackendKind` y `Profile`, struct `Discovery`, validación semántica.
- `src/proxy.rs`: `route_with_failover` (~L284), `is_retryable` (~L517), `list_models` (~L797).
- `src/discovery.rs` (**nuevo**): consulta `/models`, filtro free + tool-calling, caché con TTL, refresco en background (patrón de `src/warmup.rs`), `stale-while-error`.
- `src/classify.rs` (**nuevo**): clasificación por perfil + overrides + `min_context`.
- `src/metrics.rs`: métricas `llm_gateway_discovery_*`.

## 6. Tareas (SDD + TDD)

El detalle vivo está en `openspec/changes/add-openrouter-free-routing/tasks.md`. Resumen por fases:

1. **Configuración y tipos** — enums/structs + validación semántica (tests unitarios).
2. **Descubrimiento** — `discovery.rs`, refresco background, `stale-while-error` (tests con HTTP simulado).
3. **Clasificación** — `classify.rs`, overrides, `min_context` (tests por familia).
4. **Integración en routing** — expansión + round-robin, blacklist, fallback pago, `list_models`.
5. **Métricas** — registro e incremento de `llm_gateway_discovery_*`.
6. **Documentación** — `docs/openrouter-free-routing.md` + `gateway.yaml.example`.
7. **Verificación de integración** — suite completa, linters y E2E local.

## 7. Verificación (CLI obligatoria)

- `cargo test`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo fmt --check`
- Prueba end-to-end local con las tres rutas contra OpenRouter (failover free→pago + métricas).

## 8. Riesgos

- El tier free **no** incluye modelos top: la ruta `architect` caerá al pago con frecuencia (el fallback lo absorbe; se documenta la expectativa).
- La clasificación heurística puede errar → overrides manuales.
- El catálogo free cambia → TTL configurable + `stale-while-error`.
- Límites por-modelo agotan el pool free → rotación + blacklist con cooldown.

## 9. Criterios de cierre

- [ ] Spec OpenSpec aprobada y archivada (`openspec archive add-openrouter-free-routing`).
- [ ] Los tres perfiles funcionan end-to-end con fallback a pago.
- [ ] `cargo test` / `clippy` / `fmt --check` en verde.
- [ ] `docs/openrouter-free-routing.md` publicado.
- [ ] PR `feature/add-openrouter-free-routing` → `development` mergeado.
- [ ] Este plan renombrado a `plans/PLAN-008-ARCHIVED.md`.

## 10. Metodología

SDD (OpenSpec) + TDD (Red-Green-Refactor) + gitflow con gitmoji. Punto de entrada: issue #31. STOP obligatorio tras generar la spec, a la espera de aprobación del usuario antes de escribir código.
